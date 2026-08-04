//! S3 subagent timeline (owner screenshots wave): the parent transcript's
//! `→ messaged` marker between the spawned and report rows, the chip
//! view's `from main` sigil on parent-authored user rows, and the chip
//! composer riding S1's `agent.message` wire with a daemon-truth receipt
//! flash. Honesty twins: the demo fabricates no delivery receipt, and a
//! daemon that does not serve `agent_message_v1` refuses instead of
//! destroying the text.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentMessageDelivery, AgentMessageReceipt, AgentMessaged, AgentRole,
    ChildReport, Grant, Placement, ReportVerification,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, LeaseId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use haider_tui::app::{AppModel, AppRequest, ChipModel, Hit, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::{launcher_model, submit};

const CHILD: &str = "agent-s3-child";

fn sid() -> SessionId {
    SessionId::new("s-s3")
}

fn manifest(task: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(CHILD),
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
        lease: LeaseId::new("lease-s3"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
    }
}

/// A raw envelope with an ARBITRARY payload value — the agent-fact union
/// rides outside `EventPayload`, so the fact tests must feed the exact
/// bytes the daemon journals.
fn raw_value(seq: u64, agent: Option<&str>, payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-s3-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("s3-device"),
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
        payload,
    }
}

fn raw(seq: u64, agent: Option<&str>, payload: &EventPayload) -> RawEnvelope {
    raw_value(
        seq,
        agent,
        serde_json::to_value(payload).expect("payload serializes"),
    )
}

fn messaged_fact(preview: &str, delivery: AgentMessageDelivery) -> serde_json::Value {
    AgentMessaged {
        agent: AgentId::new(CHILD),
        preview: preview.to_owned(),
        delivery,
    }
    .to_payload_value()
    .expect("fact serializes")
}

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

/// A live session with the child chip installed from its journal manifest.
fn live_session_with_chip() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.route_raw(&raw(
        1,
        None,
        &EventPayload::AgentSpawned(manifest("stitch the timeline")),
    ));
    model.requests.clear();
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

fn note_texts(model: &AppModel) -> Vec<String> {
    model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Note { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---- (1) the main timeline's messaged marker ---------------------------

/// MUTATION CHECK: drop the `route_agent_event` fallback in
/// `AppModel::absorb_raw_active`'s decode-error arm and the fact is
/// counted unknown instead of painted — both the note assertion and the
/// unknown-payloads pin fail. Swap the delivery arms in
/// `app::messaged_note` and the `· steer` / `· queued` tails cross.
#[test]
fn agent_messaged_renders_in_main_timeline_with_delivery() {
    let mut model = live_session_with_chip();
    model.route_raw(&raw_value(
        2,
        None,
        messaged_fact("fix the tests first", AgentMessageDelivery::DeliveredSteer),
    ));
    let notes = note_texts(&model);
    let steer = notes.last().expect("the fact painted a note");
    assert!(
        steer.starts_with("→ messaged Ammar"),
        "the marker names the chip's callsign, got {steer:?}"
    );
    assert!(
        steer.contains("fix the tests first"),
        "the marker carries the bounded preview, got {steer:?}"
    );
    assert!(
        steer.ends_with("· steer"),
        "the delivery kind rides the tail, got {steer:?}"
    );
    // A payload OUTSIDE the union is consumed, never counted unknown.
    assert_eq!(model.projection.unknown_payloads(), 0);

    // The queued delivery wears its own honest tail.
    model.route_raw(&raw_value(
        3,
        None,
        messaged_fact("then run clippy", AgentMessageDelivery::DeliveredQueued),
    ));
    let notes = note_texts(&model);
    let queued = notes.last().expect("second fact painted");
    assert!(
        queued.ends_with("· queued"),
        "queued delivery says so, got {queued:?}"
    );
    assert_eq!(model.projection.unknown_payloads(), 0);

    // And the marker actually renders in the session view.
    model.screen = Screen::Session;
    let (rows, _, _) = draw(&model, 100, 30);
    row_of(&rows, "→ messaged Ammar");
}

// ---- (2) the chip view's from-main rows --------------------------------

/// MUTATION CHECK: route the chip-scoped `UserMessage` through
/// `chip.transcript.apply` instead of `session::chip_apply` and the
/// `from_main` pins fail (the rows fall back to plain ❯ user rows); drop
/// the ` · from main` tag from the renderer and the rendered-row
/// assertion fails.
#[test]
fn chip_view_shows_steer_messages() {
    let mut model = live_session_with_chip();
    // The spawn prompt and a later steer both arrive as agent-scoped user
    // messages on the parent stream — parent-authored by construction.
    model.route_raw(&raw(2, Some(CHILD), &user_message("delegated: stitch S3")));
    model.route_raw(&raw(3, Some(CHILD), &user_message("focus on fs_edit")));
    let chip = haider_tui::app::find_chip(&model.chips, CHILD).expect("chip installed");
    let marked: Vec<bool> = chip
        .transcript
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::User { from_main, .. } => Some(*from_main),
            _ => None,
        })
        .collect();
    assert_eq!(
        marked,
        vec![true, true],
        "every agent-scoped user row is marked parent-authored"
    );
    // The chip view renders the sigil and the tag at the row's position.
    model.screen = Screen::Subagent;
    model.view_path = vec![CHILD.to_owned()];
    let (rows, _, _) = draw(&model, 100, 30);
    let steer_y = row_of(&rows, "focus on fs_edit") as usize;
    assert!(
        rows[steer_y].contains('→'),
        "the steer row wears the from-main sigil, got {:?}",
        rows[steer_y]
    );
    assert!(
        rows[steer_y].contains("· from main"),
        "the steer row wears the from-main tag, got {:?}",
        rows[steer_y]
    );
}

// ---- (3) the chip composer rides the wire ------------------------------

/// MUTATION CHECK: restore the old `refuse_demo_only("steering a
/// subagent")` arm and the request never leaves the reducer; drop the
/// `AppRequest::ChipSubmit` arm in `LiveDriver::handle_request` and no
/// command is minted; swap the receipt-flash delivery arms and the
/// "steer" wording assertion fails. The no-fabrication pin fails if any
/// of those paths paints a chip row locally.
#[test]
fn chip_composer_rides_the_steer_wire_and_flashes_daemon_receipt() {
    let mut model = live_session_with_chip();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_AGENT_MESSAGE_V1.to_owned());
    model.screen = Screen::Subagent;
    model.view_path = vec![CHILD.to_owned()];
    let rows_before = haider_tui::app::find_chip(&model.chips, CHILD)
        .expect("chip installed")
        .transcript
        .entries()
        .len();

    submit(&mut model, "steer toward the failing suite");
    let request = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::ChipSubmit { agent, text } => Some((agent.clone(), text.clone())),
            _ => None,
        })
        .expect("the subagent composer emits ChipSubmit in live mode");
    assert_eq!(request.0, CHILD);
    assert_eq!(request.1, "steer toward the failing suite");

    // The driver rides S1's wire with the parent session's coordinates.
    let mut driver = LiveDriver::new("test");
    let commands = driver.handle_request(
        &mut model,
        AppRequest::ChipSubmit {
            agent: request.0,
            text: request.1,
        },
    );
    let command = commands.first().expect("one command").clone();
    let (command_id, wire_agent) = match &command {
        LiveCommand::AgentMessage {
            command_id,
            session,
            agent,
            text,
            ..
        } => {
            assert_eq!(session, &sid());
            assert_eq!(agent, CHILD);
            assert_eq!(text, "steer toward the failing suite");
            (command_id.clone(), agent.clone())
        }
        other => panic!("expected AgentMessage, got {other:?}"),
    };
    let context = CommandContext::of(&command);
    match request_body(command) {
        haider_rpc::RequestBody::AgentMessage {
            session_id, agent, ..
        } => {
            assert_eq!(session_id, sid());
            assert_eq!(agent.as_str(), wire_agent);
        }
        other => panic!("expected agent.message on the wire, got {other:?}"),
    }

    // NOTHING was painted locally while the command was in flight.
    assert_eq!(
        haider_tui::app::find_chip(&model.chips, CHILD)
            .expect("chip installed")
            .transcript
            .entries()
            .len(),
        rows_before,
        "no locally fabricated chip row — the journal facts paint the rows"
    );
    assert!(model.flash.is_none(), "no flash before the daemon answered");

    // The daemon's receipt — and ONLY the receipt — flashes the delivery.
    let receipt = AgentMessageReceipt {
        agent: AgentId::new(CHILD),
        delivery: AgentMessageDelivery::DeliveredSteer,
        child_run_id: RunId::new("run-child-s3"),
        child_run_state: RunState::Streaming,
    };
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::AgentMessage {
            receipt: receipt.clone(),
        },
    );
    assert_eq!(
        replies,
        vec![LiveReply::AgentMessaged {
            command_id,
            receipt
        }]
    );
    let reply = replies.into_iter().next().expect("one reply");
    driver.apply(&mut model, reply);
    let flash = model.flash.as_deref().expect("the receipt flashes");
    assert!(
        flash.contains("messaged Ammar") && flash.contains("steer"),
        "the flash names the chip and the daemon's delivery kind, got {flash:?}"
    );
}

// ---- (4) the timeline reads spawned → messaged → report ---------------

/// MUTATION CHECK: push the messaged note through a side surface (or
/// reorder the routing) and the strict index ordering fails — the marker
/// must sit BETWEEN the spawned and report rows of the same transcript.
#[test]
fn timeline_order_spawned_messaged_report() {
    let mut model = live_session_with_chip();
    model.route_raw(&raw(
        2,
        None,
        &EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("item-spawn"),
            item: TurnItem::ChildSpawn {
                agent: AgentId::new(CHILD),
            },
        }),
    ));
    model.route_raw(&raw_value(
        3,
        None,
        messaged_fact("land the fixture", AgentMessageDelivery::DeliveredSteer),
    ));
    model.route_raw(&raw(
        4,
        None,
        &EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("item-report"),
            item: TurnItem::ChildResult {
                report: ChildReport {
                    agent: AgentId::new(CHILD),
                    summary: "fixture landed".to_owned(),
                    verified: ReportVerification::Verified,
                    workspace_revision: None,
                },
            },
        }),
    ));
    let entries = model.projection.entries();
    let spawned = entries
        .iter()
        .position(|entry| {
            matches!(entry, TranscriptEntry::Item(block)
                if matches!(block.item, TurnItem::ChildSpawn { .. }))
        })
        .expect("spawned row");
    let messaged = entries
        .iter()
        .position(
            |entry| matches!(entry, TranscriptEntry::Note { text } if text.contains("→ messaged")),
        )
        .expect("messaged marker");
    let report = entries
        .iter()
        .position(|entry| {
            matches!(entry, TranscriptEntry::Item(block)
                if matches!(block.item, TurnItem::ChildResult { .. }))
        })
        .expect("report row");
    assert!(
        spawned < messaged && messaged < report,
        "timeline order spawned({spawned}) → messaged({messaged}) → report({report})"
    );
}

// ---- (5) honesty: the demo and an ungated daemon ----------------------

/// MUTATION CHECK: paint the delivery flash from the demo path (or let
/// the reducer emit `ChipSubmit` without the feature) and one of the two
/// halves fails — the demo half pins NO daemon-receipt flash, the ungated
/// half pins refusal with the stale-daemon note and NO request.
#[test]
fn demo_and_ungated_are_honest() {
    // (a) LIVE, daemon without `agent_message_v1`: refuse honestly —
    // nothing minted, nothing destroyed silently.
    let mut model = live_session_with_chip();
    model.screen = Screen::Subagent;
    model.view_path = vec![CHILD.to_owned()];
    submit(&mut model, "this daemon cannot hear it");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ChipSubmit { .. })),
        "no request rides a wire the daemon does not serve"
    );
    let flash = model.flash.as_deref().expect("the refusal says why");
    assert!(
        flash.contains("messaging a subagent"),
        "the refusal names the surface, got {flash:?}"
    );

    // (b) DEMO: the scripted beat may fabricate locally (that is the sim's
    // charter) but it must NOT dress anything as a daemon receipt — no
    // delivery flash exists on this path.
    let mut demo = launcher_model();
    demo.chips
        .push(ChipModel::from_manifest(&manifest("demo child")));
    demo.screen = Screen::Subagent;
    demo.view_path = vec![CHILD.to_owned()];
    submit(&mut demo, "steer the demo child");
    assert!(
        demo.requests
            .iter()
            .any(|request| matches!(request, AppRequest::ChipSubmit { .. })),
        "the demo keeps its scripted ChipSubmit beat"
    );
    assert!(
        demo.flash.is_none(),
        "the demo paints no delivery receipt — daemon truth only, got {:?}",
        demo.flash
    );
}
