//! B2b m3 — the tree screen laws (research §Q3 + the brief's m3 families):
//!
//! * TOPOLOGY: typed rows for the viewed branch, fork markers immediately
//!   under their EXACT fork node, `●` follows the session's ACTIVE branch.
//! * NAVIGATION: `/tree` opens at the root; drill/breadcrumb/esc walk
//!   parent/root and esc stays SESSION-SCOPED (never app navigation);
//!   selection clamps; a stale value-carrying hit cannot activate a
//!   replaced row.
//! * FORK: `f` emits the selected row's EXACT `{session, source branch,
//!   node, seq}` through `AppRequest::BranchCreate`; refusals are honest;
//!   completion returns the tree to the session only when daemon truth
//!   lands.
//! * SWITCH: enter on a branch/node row runs the SAME atomic
//!   `switch_branch` swap every switch takes.
//! * JUMP: a node row arms `pending_jump = {branch, node}`; the NEXT
//!   session frame resolves it through the renderer's own wrapped-row
//!   prefix sums — wrapped text, explicit newlines, wide glyphs, narrow
//!   and wide widths, resize, near-tail clamping, sticky suppression, and
//!   an unmaterialized node keeping the anchor armed.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{BranchId, DeviceId, EventId, NodeId, SessionId};
use haider_rpc::CommandId;
use haider_tui::app::{AppModel, AppRequest, Hit, PendingJump, RuntimeMode, Screen, TreeRow};
use haider_tui::app::{tree_crumb, tree_rows};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash};

fn sid() -> SessionId {
    SessionId::new("s-tree")
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
        device_id: DeviceId::new("tree-device"),
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

fn stamped(seq: u64, branch: &str, payload: &EventPayload) -> RawEnvelope {
    let mut envelope = raw(seq, payload);
    envelope.branch_id = Some(bid(branch));
    envelope
}

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

fn user_node(node: &str, text: &str) -> EventPayload {
    EventPayload::NodeCommitted(haider_protocol::history::TreeNode {
        node: NodeId::new(node),
        parent: None,
        kind: haider_protocol::history::NodeKind::UserTurn {
            text: text.to_owned(),
            attachments: vec![],
        },
    })
}

/// The daemon's `BranchCreated` journal fact for a fork of `source` at
/// exactly `(fork_node, fork_seq)`.
fn branch_created(
    seq: u64,
    branch: &str,
    name: &str,
    source: Option<&str>,
    fork_node: &str,
    fork_seq: u64,
) -> RawEnvelope {
    let created = BranchCreated {
        branch: BranchDescriptor {
            branch_id: bid(branch),
            name: name.to_owned(),
            source_branch_id: source.map(bid),
            fork_node_id: NodeId::new(fork_node),
            fork_seq,
            created_seq: seq,
            created_at_ms: 0,
            head_node_id: NodeId::new(fork_node),
            head_seq: fork_seq,
        },
    };
    let mut envelope = raw(seq, &EventPayload::IdleDecayed);
    envelope.payload = created.to_payload_value().expect("fact serializes");
    envelope
}

/// A live model with the tree session ATTACHED and the branch feature
/// served.
fn attached_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned()]
        .into_iter()
        .collect();
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model
}

/// Two committed main turns (nodes at seqs 2 and 4) and the `experiment`
/// branch forked at node-1. The two DISTINCT node/seq pairs are the
/// anti-degenerate fixture: a mutation that substitutes the tracker's
/// LAST node for a selected row's coordinates changes the emitted request.
fn seed_forked(model: &mut AppModel) {
    assert_eq!(
        model.route_raw(&raw(1, &user("T1Q alpha"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(2, &user_node("node-1", "T1Q alpha"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(3, &user("T2Q beta"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(4, &user_node("node-2", "T2Q beta"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&branch_created(5, "b-exp", "experiment", None, "node-1", 2)),
        RawOutcome::Applied
    );
    // Seeded user envelopes flip the turn engine; settle it so busy gates
    // don't shadow the laws under test.
    model.turn_active = false;
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
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

fn open_tree(model: &mut AppModel) {
    run_slash(model, "/tree");
    assert_eq!(model.screen, Screen::Tree);
}

// ---- topology ---------------------------------------------------------

/// LAW — `/tree` opens at the ROOT branch and the typed rows nest each
/// fork marker IMMEDIATELY under its exact fork node, coordinates intact.
///
/// MUTATION CHECK: append fork markers after the node walk instead of
/// under the matching node (skip the `push_forks_at` call in `tree_rows`).
/// Expected RUNTIME failure: the marker sits at index 4, not 2.
/// Verified by revert on 2026-08-03.
#[test]
fn tree_opens_at_the_root_and_nests_fork_markers_under_the_exact_fork_node() {
    let mut model = attached_model();
    seed_forked(&mut model);
    // Root view even while a fork is ACTIVE (sim tui.js:1735-1741).
    model.switch_branch(Some(&bid("b-exp")));
    open_tree(&mut model);
    let rows = tree_rows(&model);
    assert_eq!(rows.len(), 4, "branch + node + marker + node: {rows:?}");
    assert!(
        matches!(&rows[0], TreeRow::Branch { branch: None, label } if label.contains("main")),
        "row 0 is the root header: {rows:?}"
    );
    let TreeRow::Node {
        branch,
        coords,
        label,
    } = &rows[1]
    else {
        panic!("row 1 must be node-1's row: {rows:?}");
    };
    assert_eq!(branch, &None);
    assert_eq!(
        coords,
        &Some((NodeId::new("node-1"), 2)),
        "the first node row carries its exact committed coordinates"
    );
    assert!(label.contains("❯ T1Q alpha"));
    assert!(
        matches!(&rows[2], TreeRow::Fork { branch, label }
            if branch == &bid("b-exp") && label.contains("⑂ experiment")),
        "the fork marker sits IMMEDIATELY under node-1: {rows:?}"
    );
    assert!(
        matches!(&rows[3], TreeRow::Node { coords, .. }
            if coords == &Some((NodeId::new("node-2"), 4))),
        "node-2 follows with its own coordinates: {rows:?}"
    );
}

/// LAW — `●` follows the session's ACTIVE branch, `○` marks every other
/// viewed branch (sim tui.js:3392).
#[test]
fn the_active_dot_follows_the_sessions_active_branch() {
    let mut model = attached_model();
    seed_forked(&mut model);
    open_tree(&mut model);
    assert!(
        tree_rows(&model)[0].label().starts_with("● main"),
        "main is active and viewed"
    );
    model.switch_branch(Some(&bid("b-exp")));
    assert!(
        tree_rows(&model)[0].label().starts_with("○ main"),
        "main viewed while the fork is active"
    );
    model.tree_view = Some(bid("b-exp"));
    assert!(
        tree_rows(&model)[0].label().starts_with("● experiment"),
        "the fork viewed while active"
    );
}

// ---- navigation -------------------------------------------------------

/// LAW — enter on a fork marker DRILLS; the breadcrumb walks root →
/// viewed; esc climbs to the parent first and closes the screen from the
/// root — SESSION-SCOPED: it returns to the session, never the launcher.
///
/// MUTATION CHECK: make the tree's esc arm call `back_to_launcher()` at
/// the root instead of returning to the session. Expected RUNTIME
/// failure: the final screen assertion (Session, still attached).
/// Verified by revert on 2026-08-03.
#[test]
fn drill_breadcrumb_and_esc_walk_parent_then_close_session_scoped() {
    let mut model = attached_model();
    seed_forked(&mut model);
    open_tree(&mut model);
    assert_eq!(tree_crumb(&model), vec!["main"]);
    // ↓↓ onto the fork marker, ⏎ drills.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Tree, "drilling stays on the tree");
    assert_eq!(model.tree_view, Some(bid("b-exp")));
    assert_eq!(model.tree_sel, 0, "drill resets the selection");
    assert_eq!(tree_crumb(&model), vec!["main", "experiment"]);
    assert!(tree_rows(&model)[0].label().contains("esc up to parent"));
    // esc climbs to the parent, not out of the screen.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Tree);
    assert_eq!(model.tree_view, None, "esc walked up to the root");
    // esc at the root closes the screen — back to the SESSION (owner law:
    // esc never navigates the app).
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.active_session, Some(sid()), "still attached");
}

/// LAW — selection clamps to the row count, and a value-carrying hit whose
/// row was REPLACED matches nothing (it can neither select nor activate).
///
/// MUTATION CHECK: make the `Hit::TreeRow` arm activate by INDEX (use the
/// carried row only for a bounds check). Expected RUNTIME failure: the
/// ghost-row hit below moves the selection.
/// Verified by revert on 2026-08-03.
#[test]
fn selection_clamps_and_a_stale_hit_on_a_replaced_row_cannot_activate() {
    let mut model = attached_model();
    seed_forked(&mut model);
    open_tree(&mut model);
    for _ in 0..10 {
        model.handle(key(KeyCode::Down));
    }
    assert_eq!(model.tree_sel, 3, "↓ clamps at the last row");
    for _ in 0..10 {
        model.handle(key(KeyCode::Up));
    }
    assert_eq!(model.tree_sel, 0, "↑ clamps at the first row");
    // A REAL row's hit selects it.
    let rows = tree_rows(&model);
    model.handle_hit(Hit::TreeRow(rows[2].clone()));
    assert_eq!(model.tree_sel, 2, "a live row's hit selects that row");
    // A hit carrying a row no current frame contains is dropped whole.
    let before_view = model.tree_view.clone();
    model.handle_hit(Hit::TreeRow(TreeRow::Fork {
        branch: bid("b-ghost"),
        label: "  │   └⑂ ghost · ⏎ open".to_owned(),
    }));
    assert_eq!(model.tree_sel, 2, "a replaced row's hit moves nothing");
    assert_eq!(model.tree_view, before_view);
    assert_eq!(model.screen, Screen::Tree);
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. })),
        "and issues nothing"
    );
}

// ---- fork issuance ----------------------------------------------------

/// LAW — `f` emits the SELECTED row's exact coordinates, not the
/// tracker's last committed node (the fixture's two distinct node/seq
/// pairs make the difference observable).
///
/// MUTATION CHECK: make `tree_fork_selected` call
/// `self.branch_state.fork_point()` instead of the row's coords.
/// Expected RUNTIME failure: the request below carries node-2/seq-4.
/// Verified by revert on 2026-08-03.
#[test]
fn f_issues_the_selected_rows_exact_coordinates_not_the_trackers() {
    let mut model = attached_model();
    seed_forked(&mut model);
    open_tree(&mut model);
    model.handle(key(KeyCode::Down)); // node-1's row
    model.handle(key(KeyCode::Char('f')));
    let request = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::BranchCreate { .. }))
        .expect("f issues the fork");
    assert_eq!(
        request,
        &AppRequest::BranchCreate {
            session: sid(),
            source_branch: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 2,
            name: None,
        }
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· forking — the branch lands when the daemon commits it")
    );
}

/// LAW — from a DRILLED branch, `f` names that branch as the source with
/// the drilled row's own coordinates; when the daemon's receipt + journal
/// fact land, the branch activates once and the tree returns to the
/// session (sim forkAtNode's landing, driven by daemon truth).
#[test]
fn f_from_a_drilled_branch_names_that_branch_and_completion_returns_to_the_session() {
    let mut model = attached_model();
    seed_forked(&mut model);
    // The experiment branch has its own committed turn.
    assert_eq!(
        model.route_raw(&stamped(6, "b-exp", &user("E1Q fork life"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&stamped(
            6 + 1,
            "b-exp",
            &user_node("node-3", "E1Q fork life")
        )),
        RawOutcome::Applied
    );
    model.turn_active = false;
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter)); // drill into experiment
    assert_eq!(model.tree_view, Some(bid("b-exp")));
    model.handle(key(KeyCode::Down)); // node-3's row
    model.handle(key(KeyCode::Char('f')));
    let request = model
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::BranchCreate { .. }))
        .expect("f issues the fork")
        .clone();
    assert_eq!(
        request,
        AppRequest::BranchCreate {
            session: sid(),
            source_branch: Some(bid("b-exp")),
            fork_node_id: NodeId::new("node-3"),
            fork_seq: 7,
            name: None,
        }
    );
    // No local branch before daemon truth (the tree stays put) …
    assert_eq!(model.branch_state.named_count(), 1);
    assert_eq!(model.screen, Screen::Tree);
    // … the receipt arms activation, the journal fact installs + lands it.
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::BranchForked {
            command_id: CommandId::new("cmd-fork"),
            session: sid(),
            branch_id: bid("b-exp-f1"),
            name: "experiment·f1".to_owned(),
        },
    );
    assert_eq!(model.screen, Screen::Tree, "a receipt alone lands nothing");
    assert_eq!(
        model.route_raw(&branch_created(
            8,
            "b-exp-f1",
            "experiment·f1",
            Some("b-exp"),
            "node-3",
            7,
        )),
        RawOutcome::Applied
    );
    assert_eq!(model.screen, Screen::Session, "fork completion returns");
    assert_eq!(model.active_branch_name(), "experiment·f1");
}

/// LAW — every `f` refusal is honest: busy sessions wait, coordinate-free
/// rows refuse, a stale daemon is named, and the demo names daemon
/// ownership. Nothing is issued in any refusal.
#[test]
fn f_refusals_are_honest() {
    // Busy: the turn must end first.
    let mut model = attached_model();
    seed_forked(&mut model);
    model.turn_active = true;
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(
        model.flash.as_deref(),
        Some("· fork — wait for the turn to end")
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. }))
    );
    // A row without committed coordinates (no NodeCommitted seen).
    let mut model = attached_model();
    assert_eq!(
        model.route_raw(&raw(1, &user("bare row"))),
        RawOutcome::Applied
    );
    model.turn_active = false;
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(
        model.flash.as_deref(),
        Some("· fork — this row carries no committed node coordinates")
    );
    // A stale daemon is named (feature gate at the dispatch, not only in
    // /branch's grammar).
    let mut model = attached_model();
    model.daemon_features.clear();
    seed_forked(&mut model);
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('f')));
    let flash = model.flash.clone().expect("stale-daemon note");
    assert!(flash.contains("daemon"), "names the stale daemon: {flash}");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::BranchCreate { .. }))
    );
    // Demo: branches are daemon-owned. (The demo transcript gets its user
    // row through the envelope path — no driver runs in this test.)
    let mut model = launcher_model();
    model.handle(haider_tui::app::AppEvent::Envelope(Box::new(user(
        "hello tree",
    ))));
    model.turn_active = false;
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('f')));
    assert_eq!(
        model.flash.as_deref(),
        Some("· fork — live only; branches are daemon-owned")
    );
}

// ---- switch via tree --------------------------------------------------

/// LAW — enter on a BRANCH row switches through the same atomic
/// `switch_branch` swap every switch takes: transcript, tokens and chips
/// swap as one unit and the screen returns to the session at the tail.
#[test]
fn enter_on_a_branch_row_switches_via_the_same_atomic_swap() {
    let mut model = attached_model();
    seed_forked(&mut model);
    assert_eq!(
        model.route_raw(&stamped(6, "b-exp", &user("E1Q fork row"))),
        RawOutcome::Applied
    );
    model.turn_active = false;
    let main_entries = model.projection.entries().len();
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter)); // drill into experiment
    model.handle(key(KeyCode::Enter)); // enter on ITS branch header row
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.active_branch_name(), "experiment");
    assert_eq!(
        model.projection.entries().len(),
        1,
        "the fork's transcript is displayed"
    );
    assert!(
        model.pending_jump.borrow().is_none(),
        "a branch row returns at the tail — no jump armed"
    );
    // The same swap back restores main untouched.
    model.switch_branch(None);
    assert_eq!(model.projection.entries().len(), main_entries);
}

// ---- the render-resolved jump -----------------------------------------

/// Ten committed main turns with distinct texts, T5 multi-line, T3 long
/// enough to wrap even wide frames, T6 wide CJK glyphs. Seqs: turn N is
/// user seq 2N-1 / node seq 2N.
fn seed_long_history(model: &mut AppModel) {
    for turn in 1..=10u64 {
        let text = match turn {
            3 => "T3Q ".to_owned() + &"wrap ".repeat(40),
            5 => "T5Q first line\nsecond line\nthird line".to_owned(),
            6 => "T6Q ".to_owned() + &"宽字符".repeat(30),
            n => format!("T{n}Q question {n}"),
        };
        assert_eq!(
            model.route_raw(&raw(2 * turn - 1, &user(&text))),
            RawOutcome::Applied
        );
        assert_eq!(
            model.route_raw(&raw(2 * turn, &user_node(&format!("node-{turn}"), &text))),
            RawOutcome::Applied
        );
    }
    model.turn_active = false;
}

/// The buffer row index holding `marker`, if visible.
fn row_of(rows: &[String], marker: &str) -> Option<usize> {
    rows.iter().position(|row| row.contains(marker))
}

/// Arm a jump to `node` directly (the tree's Enter path is pinned
/// separately) and resolve it with one frame.
fn arm_and_draw(model: &mut AppModel, node: &str, width: u16, height: u16) -> Vec<String> {
    *model.pending_jump.borrow_mut() = Some(PendingJump {
        branch: None,
        node: NodeId::new(node),
    });
    draw(model, width, height)
}

/// The session transcript's top buffer row (2 header rows + the rule).
const TRANSCRIPT_TOP: usize = 3;

/// LAW — enter on a node row arms `{branch, node}` and the NEXT session
/// frame lands that node's prompt line at the viewport top; the anchor
/// clears; the sticky is suppressed so it cannot cover the revealed row,
/// and a real wheel lifts the suppression.
///
/// MUTATION CHECK: resolve the anchor to the entry's LOGICAL line index
/// instead of `row_of_line[line]` (skip the wrapped-row prefix sums).
/// Expected RUNTIME failure: the wrapped history above T4 shifts the
/// landing — T4Q is not at the transcript top.
/// Verified by revert on 2026-08-03.
#[test]
fn enter_on_a_node_row_lands_the_render_resolved_jump() {
    let mut model = attached_model();
    seed_long_history(&mut model);
    open_tree(&mut model);
    for _ in 0..4 {
        model.handle(key(KeyCode::Down)); // T4's node row
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(
        *model.pending_jump.borrow(),
        Some(PendingJump {
            branch: None,
            node: NodeId::new("node-4"),
        }),
        "the node row armed the durable anchor"
    );
    let rows = draw(&model, 80, 20);
    assert_eq!(
        row_of(&rows, "T4Q"),
        Some(TRANSCRIPT_TOP),
        "T4's prompt line sits at the transcript top: {rows:?}"
    );
    assert!(model.pending_jump.borrow().is_none(), "the anchor cleared");
    assert!(
        model.sticky_suppressed.get(),
        "sticky suppressed — it must not cover the revealed row"
    );
    assert!(
        row_of(&rows, "T3Q").is_none(),
        "earlier history stays above the fold"
    );
    // A REAL wheel lifts the suppression (the StickyJump law).
    model.handle_wheel(true);
    assert!(!model.sticky_suppressed.get());
}

/// LAW — the jump geometry survives wrapped text, explicit newlines, wide
/// glyphs and BOTH narrow and wide widths: every resolution runs through
/// the frame's own prefix sums, so each width lands the same node on its
/// own geometry.
#[test]
fn jump_geometry_survives_wrapping_newlines_wide_glyphs_and_widths() {
    let mut model = attached_model();
    seed_long_history(&mut model);
    // Past the wrapping T3 at a NARROW width.
    let rows = arm_and_draw(&mut model, "node-4", 48, 16);
    assert_eq!(row_of(&rows, "T4Q"), Some(TRANSCRIPT_TOP), "{rows:?}");
    // The same node at a WIDE width — fresh geometry, same landing law.
    let rows = arm_and_draw(&mut model, "node-4", 140, 20);
    assert_eq!(row_of(&rows, "T4Q"), Some(TRANSCRIPT_TOP), "{rows:?}");
    // A multi-line prompt lands on its FIRST line.
    let rows = arm_and_draw(&mut model, "node-5", 80, 16);
    assert_eq!(row_of(&rows, "T5Q first line"), Some(TRANSCRIPT_TOP));
    assert_eq!(row_of(&rows, "second line"), Some(TRANSCRIPT_TOP + 1));
    // Past the double-width CJK turn: the prefix sums must count its REAL
    // wrapped rows.
    let rows = arm_and_draw(&mut model, "node-7", 60, 14);
    assert_eq!(row_of(&rows, "T7Q"), Some(TRANSCRIPT_TOP), "{rows:?}");
}

/// LAW — a resize between arming and resolution simply resolves against
/// the NEW geometry (nothing was cached): the anchor lands at the resized
/// width, then keeps reconciling like any scroll offset.
#[test]
fn jump_resolves_after_resize_with_fresh_geometry() {
    let mut model = attached_model();
    seed_long_history(&mut model);
    *model.pending_jump.borrow_mut() = Some(PendingJump {
        branch: None,
        node: NodeId::new("node-4"),
    });
    model.handle_resize();
    let rows = draw(&model, 52, 14);
    assert_eq!(row_of(&rows, "T4Q"), Some(TRANSCRIPT_TOP), "{rows:?}");
    // A later resize reconciles the offset without panicking or banking
    // debt (render is the scroll authority).
    model.handle_resize();
    let rows = draw(&model, 120, 34);
    assert!(model.scroll_back.get() <= model.scroll_max.get());
    assert!(row_of(&rows, "T4Q").is_some() || row_of(&rows, "T10Q").is_some());
}

/// LAW — a NEAR-TAIL target cannot be top-aligned without fake padding:
/// the target row clamps to `max_scroll` (here the tail itself), the view
/// stays honest, and the target is still visible.
///
/// MUTATION CHECK: drop the `min(max_scroll)` clamp and compute
/// `max_scroll - row` unclamped. Expected RUNTIME failure: the u16
/// subtraction underflows (panic) or the tail assertion fails.
/// Verified by revert on 2026-08-03.
#[test]
fn a_near_tail_target_clamps_honestly() {
    let mut model = attached_model();
    seed_long_history(&mut model);
    let rows = arm_and_draw(&mut model, "node-10", 80, 20);
    assert_eq!(model.scroll_back.get(), 0, "the tail target folds to 0");
    assert!(row_of(&rows, "T10Q").is_some(), "the target is visible");
    assert!(
        model.pending_jump.borrow().is_none(),
        "and the anchor landed"
    );
}

/// LAW — an anchor whose node replay has NOT materialized stays armed
/// (never resolved onto a guessed entry); the catch-up that commits the
/// node resolves it on the next frame.
#[test]
fn an_unmaterialized_node_keeps_the_anchor_armed_until_catchup() {
    let mut model = attached_model();
    seed_long_history(&mut model);
    let rows = arm_and_draw(&mut model, "node-11", 80, 20);
    assert!(
        model.pending_jump.borrow().is_some(),
        "the unknown node keeps the anchor armed"
    );
    assert_eq!(model.scroll_back.get(), 0, "no guessed scroll");
    assert!(row_of(&rows, "T10Q").is_some(), "the tail keeps rendering");
    // Catch-up commits turns 11..17; the SAME armed anchor resolves on
    // the next frame — top-aligned, because enough history follows it
    // (a tail target would clamp honestly instead, its own law above).
    for turn in 11..=17u64 {
        let text = format!("T{turn}Q late arrival {turn}");
        assert_eq!(
            model.route_raw(&raw(2 * turn - 1, &user(&text))),
            RawOutcome::Applied
        );
        assert_eq!(
            model.route_raw(&raw(2 * turn, &user_node(&format!("node-{turn}"), &text))),
            RawOutcome::Applied
        );
    }
    model.turn_active = false;
    let rows = draw(&model, 80, 20);
    assert_eq!(row_of(&rows, "T11Q"), Some(TRANSCRIPT_TOP), "{rows:?}");
    assert!(model.pending_jump.borrow().is_none());
}

/// LAW — a node row on ANOTHER branch rides the same atomic switch: the
/// jump's `{branch, node}` identity swaps the displayed branch first and
/// resolves on the fork's own view.
#[test]
fn a_cross_branch_node_row_jump_rides_the_atomic_switch() {
    let mut model = attached_model();
    seed_forked(&mut model);
    // Enough fork-side content that its target is not trivially at the top.
    let mut seq = 6;
    for turn in 1..=6u64 {
        let text = format!("E{turn}Q fork question {turn}");
        assert_eq!(
            model.route_raw(&stamped(seq, "b-exp", &user(&text))),
            RawOutcome::Applied
        );
        seq += 1;
        assert_eq!(
            model.route_raw(&stamped(
                seq,
                "b-exp",
                &user_node(&format!("enode-{turn}"), &text)
            )),
            RawOutcome::Applied
        );
        seq += 1;
    }
    model.turn_active = false;
    open_tree(&mut model);
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter)); // drill into experiment
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down)); // E2's node row
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(
        model.active_branch_name(),
        "experiment",
        "the node row switched the displayed branch atomically"
    );
    let rows = draw(&model, 80, 12);
    assert_eq!(
        row_of(&rows, "E2Q"),
        Some(TRANSCRIPT_TOP),
        "the jump resolved on the fork's own view: {rows:?}"
    );
}
