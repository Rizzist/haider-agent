//! B2b m2 — `/branch` commands and the status-bar indicator.
//!
//! The picker is a numbered arrow-highlight card whose answer is
//! reducer-local (a display switch closes its own card); `new` forks at
//! the tracker's exact coordinates behind the busy() gate; every refusal
//! is an honest notice; and the feature-ungated daemon fabricates nothing.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_tui::app::{AppModel, AppRequest, BRANCH_CARD_PREFIX, RuntimeMode, Screen};
use haider_tui::projection::RawOutcome;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

fn sid() -> SessionId {
    SessionId::new("s-0")
}

fn bid(name: &str) -> BranchId {
    BranchId::new(name)
}

fn raw(seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("branch-device"),
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

fn branch_created(seq: u64, branch: &str, name: &str) -> RawEnvelope {
    let created = BranchCreated {
        branch: BranchDescriptor {
            branch_id: bid(branch),
            name: name.to_owned(),
            source_branch_id: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 1,
            created_seq: seq,
            created_at_ms: 0,
            head_node_id: NodeId::new("node-1"),
            head_seq: 1,
        },
    };
    let mut envelope = raw(seq, &EventPayload::IdleDecayed);
    envelope.payload = created.to_payload_value().expect("branch fact serializes");
    envelope
}

fn node_committed(seq: u64, node: &str) -> RawEnvelope {
    raw(
        seq,
        &EventPayload::NodeCommitted(haider_protocol::history::TreeNode {
            node: NodeId::new(node),
            parent: None,
            kind: haider_protocol::history::NodeKind::UserTurn {
                text: "committed".to_owned(),
                attachments: vec![],
            },
        }),
    )
}

/// A live session with the branch feature advertised and the `experiment`
/// branch installed by daemon truth.
fn forked_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned()]
        .into_iter()
        .collect();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(
        model.route_raw(&node_committed(1, "node-1")),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&branch_created(2, "b-exp", "experiment")),
        RawOutcome::Applied
    );
    model
}

fn picker_labels(model: &AppModel) -> Vec<String> {
    let menu = model.projection.open_menu().expect("the picker is open");
    assert!(menu.id.as_str().starts_with(BRANCH_CARD_PREFIX));
    menu.options.iter().map(|o| o.label.clone()).collect()
}

fn status_bar(model: &AppModel) -> String {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        rows.push(row);
    }
    rows.into_iter()
        .rev()
        .find(|row| row.contains(" · "))
        .unwrap_or_default()
}

// ---- gates ------------------------------------------------------------

#[test]
fn slash_branch_is_session_only_and_feature_gated() {
    // Session-only: the launcher refuses honestly.
    let mut model = launcher_model();
    submit(&mut model, "/branch");
    assert_eq!(model.flash.as_deref(), Some("· /branch — session only"));
    // Feature gate (brief item 5): a live daemon WITHOUT the advertised
    // branch feature gets the honest stale-daemon notice and NOTHING is
    // fabricated — no card, no branch, no request.
    //
    // MUTATION CHECK: drop the `daemon_serves` gate from `branch_command`
    // and the ungated daemon below opens a picker card.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.40".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    submit(&mut model, "/branch");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("needs a newer daemon")),
        "expected the stale-daemon notice, got {:?}",
        model.flash
    );
    assert!(model.projection.open_menu().is_none(), "nothing fabricated");
    submit(&mut model, "/branch new probe");
    assert!(
        model
            .requests
            .iter()
            .all(|request| !matches!(request, AppRequest::BranchCreate { .. })),
        "no fork request without the feature"
    );
}

// ---- the picker -------------------------------------------------------

#[test]
fn the_picker_lists_main_and_named_branches_with_the_active_marked() {
    // MUTATION CHECK: mark every row ○ in `branch_card` (drop the active
    // lookup) and both assertions on ● fail.
    let mut model = forked_model();
    submit(&mut model, "/branch");
    assert_eq!(picker_labels(&model), vec!["● main", "○ experiment"]);
    // Esc dismisses — session-scoped: the card closes, the screen stays.
    model.handle(key(KeyCode::Esc));
    assert!(model.projection.open_menu().is_none());
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.active_branch_name(), "main", "esc switches nothing");
    // With the fork active, the marker follows.
    model.switch_branch(Some(&bid("b-exp")));
    submit(&mut model, "/branch");
    assert_eq!(picker_labels(&model), vec!["○ main", "● experiment"]);
    model.handle(key(KeyCode::Esc));
}

#[test]
fn picker_enter_and_digits_switch_and_close_the_card_locally() {
    // MUTATION CHECK (executed for the m1 notes): route the picker's
    // answer through the outbox (drop the BRANCH_CARD_PREFIX intercept in
    // `submit_menu_answer`) and the card stays open, the switch never
    // happens, and a ghost answer sits in the outbox.
    let mut model = forked_model();
    submit(&mut model, "/branch");
    // Arrow highlight: ↓ to the fork, ⏎ activates.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert!(
        model.projection.open_menu().is_none(),
        "the card closed itself"
    );
    assert_eq!(model.active_branch_name(), "experiment");
    assert_eq!(model.flash.as_deref(), Some("· branch → experiment"));
    assert!(
        model.outbox.is_empty(),
        "a local switch never rides the outbox"
    );
    // Digits answer too: [1] returns to main.
    submit(&mut model, "/branch");
    model.handle(key(KeyCode::Char('1')));
    assert!(model.projection.open_menu().is_none());
    assert_eq!(model.active_branch_name(), "main");
    assert!(model.outbox.is_empty());
}

// ---- direct switch ----------------------------------------------------

#[test]
fn slash_branch_name_switches_directly_and_unknown_names_are_honest() {
    let mut model = forked_model();
    submit(&mut model, "/branch experiment");
    assert_eq!(model.active_branch_name(), "experiment");
    assert_eq!(model.flash.as_deref(), Some("· branch → experiment"));
    submit(&mut model, "/branch main");
    assert_eq!(model.active_branch_name(), "main");
    submit(&mut model, "/branch nope");
    assert_eq!(
        model.flash.as_deref(),
        Some("· no branch named “nope” — main · experiment"),
        "the honest refusal lists what exists"
    );
    assert_eq!(
        model.active_branch_name(),
        "main",
        "an unknown name switches nothing"
    );
}

// ---- /branch new ------------------------------------------------------

#[test]
fn slash_branch_new_gates_busy_and_empty_sessions_honestly() {
    // Busy: the sim's sessionBusy vocabulary — the /compact wording.
    let mut model = forked_model();
    model.turn_active = true;
    submit(&mut model, "/branch new");
    assert_eq!(
        model.flash.as_deref(),
        Some("· /branch new — wait for the turn to end")
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. })),
        "a busy session forks nothing"
    );
    // Idle but nothing committed: honest, no invented fork point.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned()]
        .into_iter()
        .collect();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    submit(&mut model, "/branch new");
    assert_eq!(
        model.flash.as_deref(),
        Some("· /branch new — nothing committed to fork from yet")
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. }))
    );
}

#[test]
fn slash_branch_new_issues_exact_captured_coordinates() {
    // Brief law 1: fork issuance emits exact session/source-branch/node/
    // seq — from the TRACKER, not from display rows.
    //
    // MUTATION CHECK (executed for the m1 notes): make `branch_new` send
    // `fork_seq: 0` instead of the tracker's seq and the equality below
    // fails.
    let mut model = forked_model();
    submit(&mut model, "/branch new my experiment");
    let request = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::BranchCreate { .. }))
        .expect("the fork request");
    assert_eq!(
        request,
        &AppRequest::BranchCreate {
            session: sid(),
            source_branch: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 1,
            name: Some("my experiment".to_owned()),
        }
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· forking — the branch lands when the daemon commits it")
    );
    assert_eq!(
        model.branch_state.named_count(),
        1,
        "issuance installs nothing (only the daemon fact does)"
    );
    // Forking FROM a fork: the source branch and its own last committed
    // node are captured.
    model.requests.clear();
    model.switch_branch(Some(&bid("b-exp")));
    let mut node = raw(3, &EventPayload::IdleDecayed);
    node.payload = serde_json::to_value(EventPayload::NodeCommitted(
        haider_protocol::history::TreeNode {
            node: NodeId::new("node-7"),
            parent: Some(NodeId::new("node-1")),
            kind: haider_protocol::history::NodeKind::UserTurn {
                text: "fork turn".to_owned(),
                attachments: vec![],
            },
        },
    ))
    .expect("serializes");
    node.branch_id = Some(bid("b-exp"));
    assert_eq!(model.route_raw(&node), RawOutcome::Applied);
    submit(&mut model, "/branch new");
    let request = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::BranchCreate { .. }))
        .expect("the second fork request");
    assert_eq!(
        request,
        &AppRequest::BranchCreate {
            session: sid(),
            source_branch: Some(bid("b-exp")),
            fork_node_id: NodeId::new("node-7"),
            fork_seq: 3,
            name: None,
        }
    );
}

#[test]
fn slash_branch_new_is_demo_honest_and_the_demo_picker_shows_main() {
    // The demo answers locally, so the picker opens — but it lists only
    // what exists (main; the port is single-branch) and `new` refuses:
    // branches are daemon truth and the demo has no daemon.
    let mut model = launcher_model();
    submit(&mut model, "hello demo");
    assert_eq!(model.screen, Screen::Session);
    // Settle the demo turn engine so /branch new reaches its demo gate.
    model.turn_active = false;
    submit(&mut model, "/branch new");
    assert_eq!(
        model.flash.as_deref(),
        Some("· /branch new — live only; branches are daemon-owned")
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. }))
    );
    submit(&mut model, "/branch");
    assert_eq!(picker_labels(&model), vec!["● main"]);
    model.handle(key(KeyCode::Esc));
}

// ---- the status bar ---------------------------------------------------

#[test]
fn the_status_bar_names_the_active_branch() {
    // MUTATION CHECK: hardcode the segment back to " · main" in
    // `render_status_bar` and the experiment assertion fails.
    let mut model = forked_model();
    assert!(
        status_bar(&model).contains("· main"),
        "the main branch wears its historical label"
    );
    model.switch_branch(Some(&bid("b-exp")));
    let bar = status_bar(&model);
    assert!(
        bar.contains("· experiment"),
        "the active fork's name reaches the status bar: {bar:?}"
    );
}
