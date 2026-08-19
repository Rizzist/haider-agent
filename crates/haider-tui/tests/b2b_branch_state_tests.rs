//! B2b m1 — branch state, routing, capture and RPC laws (brief §Laws).
//!
//! One session-global cursor admits everything; warm per-branch views
//! materialize beneath it from `BranchCreated` + branch-stamped envelopes;
//! every mutation captures its branch at issuance; the daemon's journal
//! fact is the only branch materializer; and the legacy main-branch wire
//! bytes stay historical.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, ChipState, Grant, Placement};
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, LeaseId, NodeId, RunId, SessionId,
};
use haider_protocol::state::RunState;
use haider_rpc::{AttachmentId, CommandId, RequestBody, ResponseBody, SubmitDisposition};
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::RawOutcome;

mod common;
use common::launcher_model;

fn sid(n: usize) -> SessionId {
    SessionId::new(format!("s-{n}"))
}

fn bid(name: &str) -> BranchId {
    BranchId::new(name)
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

/// A branch-stamped envelope — the B2a daemon stamps every accepted
/// non-main turn's batch with its branch.
fn stamped(session: &SessionId, seq: u64, branch: &str, payload: &EventPayload) -> RawEnvelope {
    let mut envelope = raw(session, seq, payload);
    envelope.branch_id = Some(bid(branch));
    envelope
}

/// The daemon's `BranchCreated` journal fact, exactly as the store commits
/// it: `branch_id: None` on the envelope, the descriptor in the payload.
fn branch_created(session: &SessionId, seq: u64, branch: &str, name: &str) -> RawEnvelope {
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
    let mut envelope = raw(session, seq, &EventPayload::IdleDecayed);
    envelope.payload = created.to_payload_value().expect("branch fact serializes");
    envelope
}

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

fn node_committed(node: &str) -> EventPayload {
    EventPayload::NodeCommitted(haider_protocol::history::TreeNode {
        node: NodeId::new(node),
        parent: None,
        kind: haider_protocol::history::NodeKind::UserTurn {
            text: "on main".to_owned(),
            attachments: vec![],
        },
    })
}

fn manifest(agent: &str) -> AgentManifest {
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
        parent: None,
        coordinates: None,
        cli_scope: None,
    }
}

/// A live model with `sid(0)` attached as the ACTIVE session.
fn attached_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid(0));
    model.open_session(&sid(0));
    model
}

/// Feed the standard fixture: main user turn (1), a committed node (2),
/// the `experiment` branch fact (3).
fn seed_branch(model: &mut AppModel) {
    assert_eq!(
        model.route_raw(&raw(&sid(0), 1, &user("on main"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&sid(0), 2, &node_committed("node-1"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&branch_created(&sid(0), 3, "b-exp", "experiment")),
        RawOutcome::Applied
    );
}

// ---- state unfold + routing ------------------------------------------

#[test]
fn branch_created_installs_a_warm_view_and_stamped_content_stays_out_of_main() {
    // MUTATION CHECK: route branch-stamped content into the live projection
    // (return `Active` for a stamped envelope in `BranchState::scope_of`)
    // and the sibling row below leaks into main's transcript.
    let mut model = attached_model();
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 4, "b-exp", &user("on the fork"))),
        RawOutcome::Applied
    );
    // Main's transcript holds ONLY its own row.
    let main_rows: Vec<_> = model.projection.entries().to_vec();
    assert_eq!(
        main_rows.len(),
        1,
        "sibling content must not leak into main"
    );
    // The warm view holds the fork's row — switching is immediate.
    assert_eq!(
        model.switch_branch(Some(&bid("b-exp"))).as_deref(),
        Some("experiment")
    );
    assert_eq!(model.projection.entries().len(), 1);
    assert_eq!(model.active_branch_name(), "experiment");
    // Switching back restores main untouched.
    assert_eq!(model.switch_branch(None).as_deref(), Some("main"));
    assert_eq!(model.projection.entries(), main_rows.as_slice());
}

#[test]
fn interleaved_branch_seqs_advance_one_cursor_and_switching_never_rewinds() {
    // Research risk 3: seq is session-global — interleaved sibling traffic
    // must advance ONE cursor with no false gaps, and a switch must not
    // reattach or rewind.
    //
    // MUTATION CHECK: drop the cursor transplant in `BranchState::switch`
    // and the post-switch duplicate below is re-applied (the fork's view
    // doubles its row) instead of rejected.
    let mut model = attached_model();
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 4, "b-exp", &user("fork row"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&sid(0), 5, &user("main again"))),
        RawOutcome::Applied
    );
    assert_eq!(model.projection.last_applied(), Some(5));
    assert_eq!(model.projection.entries().len(), 2);
    // Switch: the ONE cursor travels; no Reattach request is emitted.
    model.requests.clear();
    model.switch_branch(Some(&bid("b-exp")));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::Reattach { .. })),
        "switching must not reattach"
    );
    assert_eq!(
        model.projection.last_applied(),
        Some(5),
        "switching must not rewind"
    );
    // A redelivered envelope is still a duplicate after the switch, and
    // the next in-order envelope is not a gap.
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 4, "b-exp", &user("fork row"))),
        RawOutcome::Duplicate
    );
    assert_eq!(
        model.projection.entries().len(),
        1,
        "duplicates paint nothing"
    );
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 6, "b-exp", &user("fork row two"))),
        RawOutcome::Applied
    );
    assert_eq!(model.projection.entries().len(), 2);
}

#[test]
fn branch_state_survives_session_a_to_b_to_a_checkout() {
    // Risk 8: the branch checkout layer must ride the session checkout as
    // one atomic unit — drafts/chips/active selection cannot strand.
    //
    // MUTATION CHECK: drop the `branch_state` swap from
    // `AppModel::checkin` and the active branch (and its warm view) is
    // lost the moment another session is opened.
    let mut model = attached_model();
    model.upsert_live_session(&sid(1));
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 4, "b-exp", &user("fork row"))),
        RawOutcome::Applied
    );
    model.switch_branch(Some(&bid("b-exp")));
    // A → B → A.
    model.open_session(&sid(1));
    assert_eq!(
        model.active_branch_name(),
        "main",
        "session B has no branches"
    );
    model.open_session(&sid(0));
    assert_eq!(model.active_branch_name(), "experiment");
    assert_eq!(model.projection.entries().len(), 1);
    assert_eq!(
        model.projection.entries().len(),
        1,
        "the fork's view returns intact"
    );
}

#[test]
fn aggregate_session_state_routes_session_global_off_a_branch_stamped_stream() {
    // Risk 6 (type-first law): an aggregate `SessionState` on a
    // branch-stamped envelope lands on EVERY view — it must neither vanish
    // into one fork nor pollute a transcript.
    //
    // MUTATION CHECK: remove the `is_aggregate` arm from
    // `BranchState::scope_of` (route by branch only) and main's
    // interrupted marker below never sets.
    let mut model = attached_model();
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(
            &sid(0),
            4,
            "b-exp",
            &EventPayload::SessionState(haider_protocol::state::SessionState::Idle {
                interrupted: true
            })
        )),
        RawOutcome::Applied
    );
    assert!(
        model.projection.interrupted(),
        "the session-global idle(i) marker lands on the displayed main view"
    );
    model.switch_branch(Some(&bid("b-exp")));
    assert!(
        model.projection.interrupted(),
        "and on the warm fork view — no branch loses session status"
    );
    assert!(
        model.projection.entries().is_empty(),
        "an aggregate is never a transcript row"
    );
}

#[test]
fn render_ui_false_never_mutates_display_but_records_command_coordinates() {
    // Both halves of the exemption (brief law 6): display surfaces stay
    // untouched by `render.ui == false`, while the fork-coordinate tracker
    // — command state, not display state — records the committed node.
    //
    // MUTATION CHECK: record the tracker only under `Admission::Apply`
    // (skip the `Skip` arm in `absorb_raw_active`) and the fork point
    // below is `None`.
    let mut model = attached_model();
    let mut node = raw(&sid(0), 1, &node_committed("node-9"));
    node.render.ui = false;
    let mut hidden_user = raw(&sid(0), 2, &user("never painted"));
    hidden_user.render.ui = false;
    assert_eq!(model.route_raw(&node), RawOutcome::Applied);
    assert_eq!(model.route_raw(&hidden_user), RawOutcome::Applied);
    assert!(
        model.projection.entries().is_empty(),
        "render.ui == false still never mutates display state"
    );
    let (fork_node, fork_seq) = model
        .branch_state
        .fork_point()
        .cloned()
        .expect("the tracker recorded the ui-false NodeCommitted");
    assert_eq!(fork_node.as_str(), "node-9");
    assert_eq!(fork_seq, 1);
}

#[test]
fn inactive_branch_chips_stay_there_and_the_launcher_aggregate_counts_them() {
    // Brief law 4: an inactive branch's child chips stay in that branch's
    // view, and the launcher aggregate still counts them (a hidden live
    // child must keep its session hot).
    //
    // MUTATION CHECK: make `SessionState::live` read only the displayed
    // chips (drop `parked_live`) and the background busy assertion fails.
    let mut model = attached_model();
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(
            &sid(0),
            4,
            "b-exp",
            &EventPayload::AgentSpawned(manifest("agent-x"))
        )),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&stamped(
            &sid(0),
            5,
            "b-exp",
            &EventPayload::AgentChipState {
                agent: AgentId::new("agent-x"),
                chip: ChipState::Thinking,
            }
        )),
        RawOutcome::Applied
    );
    assert!(
        model.chips.is_empty(),
        "the inactive branch's chip must not appear on the displayed view"
    );
    // Detach: the slot's aggregate counts the parked branch's live child.
    model.back_to_launcher();
    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == sid(0))
        .expect("session slot");
    assert_eq!(slot.live(), 1, "launcher aggregate counts all branches");
    assert!(slot.busy(), "a live hidden child keeps the row hot");
    assert_eq!(slot.branches(), 2, "derived count: main + experiment");
}

#[test]
fn sibling_compaction_and_footprint_never_leak_into_the_active_branch() {
    // Brief law 4 (second half): a sibling's compaction row and token
    // meter stay in its view.
    let mut model = attached_model();
    seed_branch(&mut model);
    let usage = EventPayload::Usage(haider_protocol::provider::Usage {
        input: 900,
        output: 100,
        reasoning: 0,
        cached: 0,
        source: haider_protocol::provider::UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
    });
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 4, "b-exp", &usage)),
        RawOutcome::Applied
    );
    let compaction = EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
        item_id: haider_protocol::ids::ItemId::new("item-compact"),
        item: haider_protocol::item::TurnItem::ContextCompaction {
            summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:abc"),
            tokens_before: Some(1_000),
            tokens_after: Some(60),
        },
    });
    assert_eq!(
        model.route_raw(&stamped(&sid(0), 5, "b-exp", &compaction)),
        RawOutcome::Applied
    );
    assert_eq!(
        model.projection.context_tokens(),
        0,
        "sibling usage must not move main's meter"
    );
    assert_eq!(
        model.projection.entries().len(),
        1,
        "sibling compaction row stays out of main's transcript"
    );
    model.switch_branch(Some(&bid("b-exp")));
    assert_eq!(model.projection.context_tokens(), 1_000);
    assert_eq!(
        model.projection.entries().len(),
        1,
        "the compaction row lives in its branch"
    );
}

// ---- fork issuance + daemon truth ------------------------------------

#[test]
fn fork_issuance_travels_exact_coordinates_to_the_wire() {
    // Brief law 1: fork issuance emits EXACT session/source-branch/node/
    // seq, through AppRequest → LiveCommand → RequestBody unchanged.
    //
    // MUTATION CHECK: make `LiveDriver::handle_request`'s BranchCreate arm
    // substitute the session's LAST NodeCommitted for the captured one and
    // the request-body equality below fails.
    let mut model = attached_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        model_ref(&mut model),
        LiveReply::Attached {
            session: sid(0),
            attachment: AttachmentId::new("att-0"),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    let commands = driver.handle_request(
        &mut model,
        AppRequest::BranchCreate {
            session: sid(0),
            source_branch: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 2,
            name: Some("experiment".to_owned()),
        },
    );
    let command = commands.first().expect("one fork command").clone();
    let LiveCommand::BranchCreate {
        command_id,
        session,
        worker_generation,
        source_branch,
        fork_node_id,
        fork_seq,
        name,
    } = command.clone()
    else {
        panic!("expected LiveCommand::BranchCreate, got {command:?}");
    };
    assert_eq!(session, sid(0));
    assert_eq!(worker_generation, 7);
    assert_eq!(source_branch, None);
    assert_eq!(fork_node_id.as_str(), "node-1");
    assert_eq!(fork_seq, 2);
    assert_eq!(name.as_deref(), Some("experiment"));
    assert_eq!(
        request_body(command),
        RequestBody::BranchCreate {
            command_id,
            session_id: sid(0),
            worker_generation: 7,
            source_branch_id: None,
            fork_node_id: NodeId::new("node-1"),
            fork_seq: 2,
            name: Some("experiment".to_owned()),
        }
    );
}

/// Identity helper so a driver call and a later borrow read one model.
fn model_ref(model: &mut AppModel) -> &mut AppModel {
    model
}

#[test]
fn no_live_branch_before_daemon_truth_and_the_journal_fact_activates_once() {
    // Brief law 1: receipt correlation installs NOTHING; the daemon's
    // `BranchCreated` fact is the only materializer, and a replayed
    // success installs one branch.
    //
    // MUTATION CHECK: make the driver's `BranchForked` arm install the
    // branch into the registry itself and the "nothing installed" half
    // below fails.
    let mut model = attached_model();
    let mut driver = LiveDriver::new("test");
    seed_user_and_node(&mut model);
    let forked = LiveReply::BranchForked {
        command_id: CommandId::new("cmd-fork"),
        session: sid(0),
        branch_id: bid("b-exp"),
        name: "experiment".to_owned(),
    };
    driver.apply(&mut model, forked.clone());
    assert!(
        !model.branch_state.contains(&bid("b-exp")),
        "the receipt must install nothing"
    );
    assert_eq!(model.active_branch_name(), "main");
    // The journal fact lands: install + the armed activation fires.
    assert_eq!(
        model.route_raw(&branch_created(&sid(0), 3, "b-exp", "experiment")),
        RawOutcome::Applied
    );
    assert_eq!(model.active_branch_name(), "experiment");
    // Replay of the same fact (fresh seq, at-least-once world): ONE branch.
    assert_eq!(
        model.route_raw(&branch_created(&sid(0), 4, "b-exp", "experiment")),
        RawOutcome::Applied
    );
    assert_eq!(
        model.branch_state.named_count(),
        1,
        "replay installs one branch"
    );
    // A replayed receipt after daemon truth switches (idempotently) and
    // still installs nothing new.
    driver.apply(&mut model, forked);
    assert_eq!(model.branch_state.named_count(), 1);
}

fn seed_user_and_node(model: &mut AppModel) {
    assert_eq!(
        model.route_raw(&raw(&sid(0), 1, &user("on main"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&sid(0), 2, &node_committed("node-1"))),
        RawOutcome::Applied
    );
}

#[test]
fn a_typed_fork_failure_leaves_topology_unchanged() {
    // Brief law 1 (last half): a typed failure must not touch the
    // registry, the active selection, or any view.
    let mut model = attached_model();
    let mut driver = LiveDriver::new("test");
    seed_user_and_node(&mut model);
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(CommandId::new("cmd-fork")),
            code: "invalid_argument".to_owned(),
            message: "fork node is not on the source lineage".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert_eq!(model.branch_state.named_count(), 0);
    assert_eq!(model.active_branch_name(), "main");
    assert_eq!(
        model.projection.entries().len(),
        1,
        "the transcript is untouched"
    );
}

#[test]
fn a_fork_receipt_activates_only_the_originating_session() {
    // Brief law 1: "replayed success … activates only the originating
    // session" — a fork for background session A must not switch the
    // attached session B.
    let mut model = attached_model();
    model.upsert_live_session(&sid(1));
    // The fact for background session 1 materializes its branch.
    {
        let slot = model
            .sessions
            .iter_mut()
            .find(|entry| entry.id == sid(1))
            .expect("slot 1");
        assert_eq!(
            slot.absorb_raw(&branch_created(&sid(1), 1, "b-bg", "background")),
            RawOutcome::Applied
        );
    }
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::BranchForked {
            command_id: CommandId::new("cmd-bg"),
            session: sid(1),
            branch_id: bid("b-bg"),
            name: "background".to_owned(),
        },
    );
    assert_eq!(
        model.active_branch_name(),
        "main",
        "the attached session must not switch for another session's fork"
    );
    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == sid(1))
        .expect("slot 1");
    assert_eq!(
        slot.branch_state.active().map(BranchId::as_str),
        Some("b-bg"),
        "the originating session activates its fork"
    );
}

// ---- capture at issuance ---------------------------------------------

#[test]
fn a_submit_queued_before_a_later_switch_still_carries_its_captured_branch() {
    // Brief law 2 (second half) / research risk 4: the branch is captured
    // at ISSUANCE; a later switch must not retarget the queued turn.
    //
    // MUTATION CHECK (executed for the m1 notes): make the live driver's
    // `SubmitText` arm re-read `model.branch_state.active()` instead of
    // the request's captured branch and this test fails — the submit
    // lands on main.
    let mut model = attached_model();
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(0),
            attachment: AttachmentId::new("att-0"),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    seed_branch(&mut model);
    model.switch_branch(Some(&bid("b-exp")));
    // The user submits ON the fork…
    common::submit(&mut model, "run the experiment");
    let request = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::SubmitText { .. }))
        .expect("a submit request");
    let AppRequest::SubmitText { branch, .. } = &request else {
        unreachable!()
    };
    assert_eq!(branch.as_ref().map(BranchId::as_str), Some("b-exp"));
    // …then switches BEFORE the drain.
    model.switch_branch(None);
    let commands = driver.handle_request(&mut model, request);
    let Some(LiveCommand::Submit { branch, .. }) = commands.first() else {
        panic!("expected a Submit command, got {commands:?}");
    };
    assert_eq!(
        branch.as_ref().map(BranchId::as_str),
        Some("b-exp"),
        "the captured branch survives the switch"
    );
}

#[test]
fn menu_answers_and_compact_capture_the_branch_at_issuance() {
    let mut model = attached_model();
    seed_branch(&mut model);
    assert_eq!(
        model.route_raw(&stamped(
            &sid(0),
            4,
            "b-exp",
            &EventPayload::MenuOpened(haider_protocol::menu::Menu {
                id: haider_protocol::ids::MenuId::new("card-1"),
                kind: haider_protocol::menu::MenuKind::Question,
                title: "pick".to_owned(),
                body: vec![],
                options: vec![haider_protocol::menu::MenuOption {
                    key: "yes".to_owned(),
                    label: "yes".to_owned(),
                    detail: None,
                    decision: None,
                }],
                blocking: true,
                scope: haider_protocol::menu::MenuScope::Session,
                origin: "test".to_owned(),
                ttl_ms: None,
                timeout_option: None,
            })
        )),
        RawOutcome::Applied
    );
    model.switch_branch(Some(&bid("b-exp")));
    // Answer the fork's card while the fork is displayed.
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
    let answer = model.outbox.last().expect("an outbound answer");
    assert_eq!(answer.branch.as_ref().map(BranchId::as_str), Some("b-exp"));
    // A later switch does not rewrite the captured coordinate.
    model.switch_branch(None);
    let answer = model.outbox.last().expect("still queued");
    assert_eq!(answer.branch.as_ref().map(BranchId::as_str), Some("b-exp"));
    // The COMMITTED resolution closes the card (live discipline: the
    // envelope, never the local answer, closes it) …
    assert_eq!(
        model.route_raw(&stamped(
            &sid(0),
            5,
            "b-exp",
            &EventPayload::MenuAnswered(haider_protocol::menu::MenuAnswer {
                menu: haider_protocol::ids::MenuId::new("card-1"),
                option_key: Some("yes".to_owned()),
                option_index: 0,
                value: None,
                via: haider_protocol::menu::AnswerVia::Tui,
            })
        )),
        RawOutcome::Applied
    );
    // … the turn settles (the /compact gate is idle-only) …
    assert_eq!(
        model.route_raw(&raw(&sid(0), 6, &EventPayload::RunState(RunState::Done))),
        RawOutcome::Applied
    );
    // … so /compact issued FROM the fork captures the fork.
    model.switch_branch(Some(&bid("b-exp")));
    common::submit(&mut model, "/compact");
    let compact = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::Compact { branch } => Some(branch.clone()),
            _ => None,
        })
        .expect("a compact request");
    assert_eq!(compact.as_ref().map(BranchId::as_str), Some("b-exp"));
}

// ---- wire encode/decode selection ------------------------------------

#[test]
fn main_branch_wire_bytes_stay_historical_and_branches_ride_the_decode_forms() {
    // Brief item 3: `None` encodes the LEGACY TurnSubmit/SessionCompact
    // forms — byte-identical to the pre-branch wire — while a captured
    // branch encodes the branch-capable decode forms. The VARIANT is
    // pinned as well as the bytes: `TurnSubmitWithBranch { branch_id:
    // None }` happens to serialize identically (serde skips the `None`),
    // so a byte check alone cannot see the encode-selection law drift
    // away from the brief's letter (executed-mutation finding).
    let submit = |branch: Option<BranchId>| LiveCommand::Submit {
        command_id: CommandId::new("cmd-1"),
        session: sid(0),
        worker_generation: 7,
        text: "hi".to_owned(),
        mode: haider_protocol::DeliveryMode::Steer,
        branch,
        attachments: vec![],
    };
    assert!(
        matches!(request_body(submit(None)), RequestBody::TurnSubmit { .. }),
        "main submits ride the LEGACY variant, not a None-branch decode form"
    );
    assert!(
        matches!(
            request_body(submit(Some(bid("b-exp")))),
            RequestBody::TurnSubmitWithBranch { .. }
        ),
        "captured branches ride the branch-capable decode form"
    );
    let main = serde_json::to_value(request_body(submit(None))).expect("encodes");
    assert_eq!(
        main.get("method").and_then(|v| v.as_str()),
        Some("turn.submit")
    );
    assert!(
        main.get("branch_id").is_none(),
        "main submit bytes must stay historical: {main}"
    );
    let forked = serde_json::to_value(request_body(submit(Some(bid("b-exp"))))).expect("encodes");
    assert_eq!(
        forked.get("branch_id").and_then(|v| v.as_str()),
        Some("b-exp")
    );
    // Compaction: same selection law.
    let compact = |branch: Option<BranchId>| LiveCommand::Compact {
        command_id: CommandId::new("cmd-2"),
        session: sid(0),
        worker_generation: 7,
        branch,
    };
    assert!(
        matches!(
            request_body(compact(None)),
            RequestBody::SessionCompact { .. }
        ),
        "main compaction rides the LEGACY variant"
    );
    let main = serde_json::to_value(request_body(compact(None))).expect("encodes");
    assert!(
        main.get("branch_id").is_none(),
        "main compact bytes stay historical"
    );
    let forked = serde_json::to_value(request_body(compact(Some(bid("b-exp"))))).expect("encodes");
    assert_eq!(
        forked.get("branch_id").and_then(|v| v.as_str()),
        Some("b-exp")
    );
    // Cancel: the branch is client-side capture only — the wire cancel is
    // unchanged in both cases.
    let cancel = serde_json::to_value(request_body(LiveCommand::Cancel {
        command_id: CommandId::new("cmd-3"),
        session: sid(0),
        worker_generation: 7,
        run_id: RunId::new("run-1"),
        branch: Some(bid("b-exp")),
    }))
    .expect("encodes");
    assert!(
        cancel.get("branch_id").is_none(),
        "cancel wire bytes are unchanged"
    );
}

#[test]
fn branch_responses_map_to_their_driver_facts() {
    // The branch-pinned response shapes carry the same driver facts as the
    // legacy ones; `branch.create` maps to the fork receipt.
    let submit = CommandContext::of(&LiveCommand::Submit {
        command_id: CommandId::new("cmd-1"),
        session: sid(0),
        worker_generation: 7,
        text: "hi".to_owned(),
        mode: haider_protocol::DeliveryMode::Steer,
        branch: Some(bid("b-exp")),
        attachments: vec![],
    });
    assert_eq!(
        map_response(
            &submit,
            ResponseBody::TurnSubmitOnBranch {
                session_id: sid(0),
                run_id: RunId::new("run-1"),
                accepted_seq: 9,
                worker_generation: 7,
                branch_id: bid("b-exp"),
                disposition: SubmitDisposition::Started,
            }
        ),
        vec![LiveReply::Submitted {
            command_id: CommandId::new("cmd-1"),
            session: sid(0),
            worker_generation: 7,
            disposition: SubmitDisposition::Started,
        }]
    );
    let fork = CommandContext::of(&LiveCommand::BranchCreate {
        command_id: CommandId::new("cmd-2"),
        session: sid(0),
        worker_generation: 7,
        source_branch: None,
        fork_node_id: NodeId::new("node-1"),
        fork_seq: 2,
        name: Some("experiment".to_owned()),
    });
    assert_eq!(
        map_response(
            &fork,
            ResponseBody::BranchCreate {
                session_id: sid(0),
                branch_id: bid("b-exp"),
                source_branch_id: None,
                fork_node_id: NodeId::new("node-1"),
                fork_seq: 2,
                created_seq: 3,
                worker_generation: 7,
                name: "experiment".to_owned(),
            }
        ),
        vec![LiveReply::BranchForked {
            command_id: CommandId::new("cmd-2"),
            session: sid(0),
            branch_id: bid("b-exp"),
            name: "experiment".to_owned(),
        }]
    );
    let compact = CommandContext::of(&LiveCommand::Compact {
        command_id: CommandId::new("cmd-3"),
        session: sid(0),
        worker_generation: 7,
        branch: Some(bid("b-exp")),
    });
    assert_eq!(
        map_response(
            &compact,
            ResponseBody::SessionCompactOnBranch {
                session_id: sid(0),
                run_id: RunId::new("run-2"),
                accepted_seq: 11,
                worker_generation: 7,
                branch_id: bid("b-exp"),
            }
        ),
        vec![LiveReply::Compacted {
            command_id: CommandId::new("cmd-3"),
        }]
    );
}

// ---- background slots -------------------------------------------------

#[test]
fn background_slots_materialize_branch_views_through_the_same_router() {
    // Brief item 2: warm views for background sessions too — switching a
    // session in, then switching its branch, shows content that arrived
    // while it was detached.
    let mut model = attached_model();
    model.upsert_live_session(&sid(1));
    {
        let slot = model
            .sessions
            .iter_mut()
            .find(|entry| entry.id == sid(1))
            .expect("slot 1");
        assert_eq!(
            slot.absorb_raw(&raw(&sid(1), 1, &user("bg main"))),
            RawOutcome::Applied
        );
        assert_eq!(
            slot.absorb_raw(&branch_created(&sid(1), 2, "b-bg", "background")),
            RawOutcome::Applied
        );
        assert_eq!(
            slot.absorb_raw(&stamped(&sid(1), 3, "b-bg", &user("bg fork"))),
            RawOutcome::Applied
        );
        assert_eq!(
            slot.projection.entries().len(),
            1,
            "slot main view holds its row"
        );
    }
    model.open_session(&sid(1));
    assert_eq!(model.projection.entries().len(), 1);
    assert_eq!(
        model.switch_branch(Some(&bid("b-bg"))).as_deref(),
        Some("background")
    );
    assert_eq!(
        model.projection.entries().len(),
        1,
        "the background fork view is warm"
    );
    assert_eq!(
        model.projection.last_applied(),
        Some(3),
        "one continuous cursor"
    );
}
