//! escretract-ui (v0.0.970 owner QoL, 2026-09-03) — Esc BEFORE the first
//! response token retracts the prompt back into the composer.
//!
//! The daemon/contract half already landed: `turn.retract`, the durable
//! `prompt_retracted` fact, the typed `too_late` arbitration and the
//! transcript projection that hides the exact retracted node. What is pinned
//! here is the CLIENT half — which key issues which command, what the
//! composer gets back, what protects the user from resending a prompt that is
//! still in flight, and that none of it invents state the daemon did not
//! commit.
//!
//! The `LiveDriver` is a pure state machine, so every law below is pinned
//! without a daemon; the wire shapes go through the real `link` mapping so a
//! rename on either side fails here rather than in production.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, NodeId, RunId, SessionId};
use haider_protocol::state::RunState;
use haider_protocol::tool::AttachmentBlock;
use haider_rpc::{AttachmentId, CommandId, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, DraftKey, RuntimeMode, Screen};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::TranscriptEntry;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

const RUN: &str = "run-escretract-1";
const NODE: &str = "node-escretract-1";
const PROMPT: &str = "rewrite the boundary test";
const PROMPT_SEQ: u64 = 1;

fn sid() -> SessionId {
    SessionId::new("s-escretract")
}

fn attachment_id() -> AttachmentId {
    AttachmentId::new("att-escretract")
}

fn summary() -> SessionSummary {
    SessionSummary {
        session_id: sid(),
        head_seq: 0,
        worker_generation: 7,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
        forked_from: None,
    }
}

/// One raw envelope. `ui` is a parameter because the first-response boundary
/// is committed `render.ui == false` on purpose — a display surface must never
/// move for it — and the whole point of this lane is that the client still
/// LEARNS from it.
fn raw(seq: u64, payload: serde_json::Value, ui: bool) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: Some(RunId::new(RUN)),
        agent_id: None,
        device_id: DeviceId::new("live-device"),
        authority_epoch: 1,
        worker_generation: 9,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}

fn event(payload: &EventPayload, seq: u64) -> RawEnvelope {
    raw(
        seq,
        serde_json::to_value(payload).expect("payload serializes"),
        true,
    )
}

fn image_block() -> AttachmentBlock {
    AttachmentBlock::Image {
        artifact: ArtifactRef::new(format!("blake3:{:0>64}", "escretract")),
        mime: "image/png".to_owned(),
        width: Some(800),
        height: Some(600),
    }
}

/// The exact durable fact the daemon commits when a retraction wins.
fn retraction_fact() -> serde_json::Value {
    haider_protocol::retraction::PromptRetractedV1 {
        prompt_seq: PROMPT_SEQ,
        prompt_node_id: NodeId::new(NODE),
        text: PROMPT.to_owned(),
        attachments: vec![image_block()],
    }
    .to_payload_value()
    .expect("retraction fact")
}

/// The daemon's first-response boundary — `response_started`, `ui == false`.
fn boundary(seq: u64) -> RawEnvelope {
    raw(
        seq,
        haider_protocol::retraction::response_started_payload(
            serde_json::json!({"type": "text_delta", "text": "Sure"}),
        ),
        false,
    )
}

fn deliver(driver: &mut LiveDriver, model: &mut AppModel, envelope: RawEnvelope) {
    driver.apply(
        model,
        LiveReply::Event {
            attachment: attachment_id(),
            session: sid(),
            envelope: Box::new(envelope),
        },
    );
}

/// A live session, attached and open, with an accepted prompt whose run is
/// live and whose first response has NOT arrived — the exact state the owner's
/// QoL request is about.
fn accepted_turn() -> (LiveDriver, AppModel) {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TURN_RETRACT_V1.to_owned());
    let mut driver = LiveDriver::new("escretract-test");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![summary()],
            next_cursor: None,
        },
    );
    driver.ensure_attached(&model, &sid());
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(),
            attachment: attachment_id(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    model.open_session(&sid());

    // The committed prompt, its durable node anchor, then the live run.
    deliver(
        &mut driver,
        &mut model,
        event(
            &EventPayload::UserMessage {
                text: PROMPT.to_owned(),
                attachments: vec![image_block()],
                mode: haider_protocol::DeliveryMode::Steer,
            },
            PROMPT_SEQ,
        ),
    );
    deliver(
        &mut driver,
        &mut model,
        event(
            &EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new(NODE),
                parent: None,
                kind: NodeKind::UserTurn {
                    text: PROMPT.to_owned(),
                    attachments: vec![image_block()],
                },
            }),
            2,
        ),
    );
    deliver(
        &mut driver,
        &mut model,
        event(&EventPayload::RunState(RunState::Streaming), 3),
    );
    model.requests.clear();
    (driver, model)
}

fn user_rows(model: &AppModel) -> Vec<String> {
    model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Press Esc and drain the reducer's request into the driver, returning the
/// commands it issued.
fn esc(driver: &mut LiveDriver, model: &mut AppModel) -> Vec<LiveCommand> {
    model.handle(key(KeyCode::Esc));
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    requests
        .into_iter()
        .flat_map(|request| driver.handle_request(model, request))
        .collect()
}

#[test]
fn esc_before_the_first_token_retracts_and_restores_text_and_attachments() {
    // The owner's request end to end: the turn is cancelled, the prompt row
    // leaves the transcript, and the exact bytes — text AND attachments — come
    // back to the composer to edit and resend.
    //
    // MUTATION CHECK: make `AppModel::can_retract_prompt` return `false` and
    // the command below is a `Cancel`, failing the retract assertion; make it
    // ignore `response_started` and
    // `a_committed_response_boundary_makes_esc_a_plain_cancel` fails instead.
    let (mut driver, mut model) = accepted_turn();
    assert!(model.turn_active, "the accepted turn is live");
    assert_eq!(user_rows(&model), [PROMPT], "the prompt is on screen");
    assert!(model.can_retract_prompt(), "no response has arrived yet");

    let commands = esc(&mut driver, &mut model);
    assert!(
        model.retract_pending,
        "the outcome is unknown until the daemon answers"
    );
    assert!(!model.turn_active, "the turn is over either way");
    let Some(command @ LiveCommand::Retract { .. }) = commands.first() else {
        panic!("Esc before the first token must retract, got {commands:?}");
    };
    let LiveCommand::Retract {
        command_id,
        session,
        run_id,
        ..
    } = command.clone()
    else {
        unreachable!("matched above")
    };
    assert_eq!(session, sid());
    assert_eq!(run_id, RunId::new(RUN), "the run the stream named");

    // The real wire shape — not a hand-written frame.
    let context = CommandContext::of(command);
    assert!(
        matches!(
            request_body(command.clone()),
            haider_rpc::RequestBody::TurnRetract { run_id, session_id, .. }
                if run_id == RunId::new(RUN) && session_id == sid()
        ),
        "turn.retract, never turn.cancel"
    );

    // The daemon commits the fact on the stream; the projection drops the
    // exact node. This is what makes live and replay agree.
    deliver(&mut driver, &mut model, raw(4, retraction_fact(), true));
    assert!(
        user_rows(&model).is_empty(),
        "the retracted prompt leaves the transcript"
    );

    // Then the receipt, mapped through the real response path.
    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::TurnRetract {
            session_id: sid(),
            run_id: RunId::new(RUN),
            prompt_seq: PROMPT_SEQ,
            retracted_seq: 4,
            text: PROMPT.to_owned(),
            attachments: vec![image_block()],
        },
    );
    assert!(
        matches!(
            replies.first(),
            Some(LiveReply::PromptRetracted { command_id: id, .. }) if *id == command_id
        ),
        "the receipt correlates by its own command id, got {replies:?}"
    );
    for reply in replies {
        driver.apply(&mut model, reply);
    }

    assert_eq!(
        model.composer.text(),
        PROMPT,
        "the prompt is editable again"
    );
    assert_eq!(
        model.composer.cursor(),
        PROMPT.len(),
        "cursor at the end, ready to edit"
    );
    assert_eq!(
        model.composer.attachments().len(),
        1,
        "the attachment chip is restored from the CAS reference"
    );
    assert_eq!(
        model.composer.attachments()[0].ready_block(),
        Some(image_block()),
        "the chip carries the daemon's exact block — no re-upload"
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· prompt retracted — edit and resend")
    );
    assert!(
        !model.retract_pending,
        "the window is closed; sending is possible again"
    );
}

#[test]
fn too_late_falls_back_to_one_plain_cancel_and_keeps_the_prompt_on_screen() {
    // The race the daemon arbitrates: the response won the durable writer
    // race, so the prompt is NOT retractable. Esc must then behave exactly as
    // it always has — one ordinary cancel for the same run, the message and
    // any partial reply staying put — and it must cost exactly one extra
    // round trip, never a second retract attempt.
    //
    // MUTATION CHECK: delete the `retract_flight` arm at the top of the
    // `Failed` handler and no cancel is issued at all — Esc silently does
    // nothing and `retract_pending` strands the composer.
    let (mut driver, mut model) = accepted_turn();
    let commands = esc(&mut driver, &mut model);
    let Some(LiveCommand::Retract { command_id, .. }) = commands.first().cloned() else {
        panic!("expected a retract, got {commands:?}");
    };

    let fallback = driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: haider_rpc::ERROR_CODE_TOO_LATE.to_owned(),
            message: "the response already committed".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(
        matches!(
            fallback.as_slice(),
            [LiveCommand::Cancel { run_id, session, .. }]
                if *run_id == RunId::new(RUN) && *session == sid()
        ),
        "exactly one ordinary cancel for the same run, got {fallback:?}"
    );
    assert_eq!(
        user_rows(&model),
        [PROMPT],
        "a too-late Esc keeps the message exactly as a plain cancel would"
    );
    assert!(model.composer.is_empty(), "nothing is restored");
    assert_eq!(
        model.flash.as_deref(),
        Some("· too late to retract — cancelled")
    );
    assert!(
        !model.retract_pending,
        "with no prompt coming back there is nothing left to guard"
    );
}

#[test]
fn an_unexpected_retract_failure_still_cancels_and_names_the_error() {
    // "On any other error: plain cancel + the error in the status line; never
    // lose the user's text." An unexpected code must not leave the run alive
    // after the user asked for it to stop.
    let (mut driver, mut model) = accepted_turn();
    let commands = esc(&mut driver, &mut model);
    let Some(LiveCommand::Retract { command_id, .. }) = commands.first().cloned() else {
        panic!("expected a retract, got {commands:?}");
    };

    let fallback = driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: "internal".to_owned(),
            message: "writer unavailable".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(
        matches!(fallback.as_slice(), [LiveCommand::Cancel { run_id, .. }]
            if *run_id == RunId::new(RUN)),
        "an unexpected failure still cancels the run, got {fallback:?}"
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· retract failed — cancelled · writer unavailable"),
        "the reason is public, not swallowed"
    );
    assert!(!model.retract_pending);
    assert_eq!(user_rows(&model), [PROMPT], "the transcript is untouched");
}

#[test]
fn enter_cannot_resend_a_prompt_while_its_retraction_is_still_in_flight() {
    // The race the brief names: between Esc and the daemon's answer the prompt
    // may still be accepted, so a second Enter would resend a turn that is
    // already running. Submit is refused for exactly that window — and the
    // text the user typed into it is never lost.
    //
    // MUTATION CHECK: drop the `retract_pending` guard from
    // `submit_composer` and the `SubmitText` assertion below fails.
    let (mut driver, mut model) = accepted_turn();
    let commands = esc(&mut driver, &mut model);
    let Some(LiveCommand::Retract { command_id, .. }) = commands.first().cloned() else {
        panic!("expected a retract, got {commands:?}");
    };
    assert!(model.retract_pending);

    for character in "second".chars() {
        model.handle(key(KeyCode::Char(character)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model.requests.is_empty(),
        "no turn may be submitted while the retraction is unresolved, got {:?}",
        model.requests
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· retracting the prompt — a moment")
    );
    assert_eq!(
        model.composer.text(),
        "second",
        "the refusal happens BEFORE the take — the draft survives"
    );

    // The receipt then restores the prompt without discarding what was typed
    // into the guarded window.
    driver.apply(
        &mut model,
        LiveReply::PromptRetracted {
            command_id,
            session: sid(),
            run_id: RunId::new(RUN),
            text: PROMPT.to_owned(),
            attachments: Vec::new(),
        },
    );
    assert_eq!(
        model.composer.text(),
        format!("{PROMPT}\nsecond"),
        "the retraction never costs the user text — including text it did not author"
    );
    assert!(!model.retract_pending, "the guard lifts with the receipt");

    // And now Enter works again.
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "sending is possible once the retraction resolved"
    );
}

#[test]
fn a_committed_response_boundary_makes_esc_a_plain_cancel() {
    // The window closes on the daemon's own authority — the `response_started`
    // fact it arbitrates retraction against — and NOT on a guess from rendered
    // rows. The fact is committed `render.ui == false`, so this also pins that
    // a skipped envelope still teaches the client something.
    //
    // MUTATION CHECK: remove the `route_response_boundary` call from
    // `absorb_raw_active` and Esc keeps retracting after the first token.
    let (mut driver, mut model) = accepted_turn();
    assert!(model.can_retract_prompt(), "still open before the boundary");

    deliver(&mut driver, &mut model, boundary(4));
    assert!(
        model.projection.response_started(),
        "the ui==false boundary is recorded as turn-control truth"
    );
    assert!(
        user_rows(&model) == [PROMPT] && model.projection.entries().len() == 1,
        "and no display surface moved for it"
    );
    assert!(
        !model.can_retract_prompt(),
        "once a token has landed the prompt is no longer retractable"
    );

    let commands = esc(&mut driver, &mut model);
    assert!(
        matches!(commands.as_slice(), [LiveCommand::Cancel { run_id, .. }]
            if *run_id == RunId::new(RUN)),
        "Esc after the first token is a plain cancel, got {commands:?}"
    );
    assert!(
        !model.retract_pending,
        "a plain cancel never guards the composer"
    );
    assert_eq!(
        user_rows(&model),
        [PROMPT],
        "the message stays, exactly as it always has"
    );
}

#[test]
fn a_new_turn_reopens_the_retraction_window_the_previous_response_closed() {
    // The boundary is PER RUN. Without the reset at the turn-opening edge one
    // answered turn would make every later Esc a plain cancel for the rest of
    // the session.
    //
    // MUTATION CHECK: delete `self.response_started = false` from the opening
    // edge in `SessionProjection::apply` and this fails.
    let (mut driver, mut model) = accepted_turn();
    deliver(&mut driver, &mut model, boundary(4));
    deliver(
        &mut driver,
        &mut model,
        event(&EventPayload::RunState(RunState::Done), 5),
    );
    assert!(!model.can_retract_prompt(), "that turn was answered");

    deliver(
        &mut driver,
        &mut model,
        event(&EventPayload::RunState(RunState::Streaming), 6),
    );
    assert!(
        model.can_retract_prompt(),
        "a genuinely new turn starts with its window open again"
    );
}

#[test]
fn a_retracted_prompt_is_gone_from_replay_and_from_the_esc_esc_chooser() {
    // Reopening the session must not show the retracted prompt anywhere: not
    // in the transcript rebuilt from the journal, and not in prompt history —
    // a separate surface with its own durable coordinates, whose entry could
    // only disappoint (a fork at its hidden cut is refused).
    //
    // MUTATION CHECK: delete the `forget_retracted_prompt` call in
    // `absorb_raw_active` and the history assertion fails while the transcript
    // one still passes — they are genuinely different surfaces.
    let stream = |model: &mut AppModel, driver: &mut LiveDriver| {
        deliver(
            driver,
            model,
            event(
                &EventPayload::UserMessage {
                    text: PROMPT.to_owned(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Steer,
                },
                PROMPT_SEQ,
            ),
        );
        deliver(
            driver,
            model,
            event(
                &EventPayload::NodeCommitted(TreeNode {
                    node: NodeId::new(NODE),
                    parent: None,
                    kind: NodeKind::UserTurn {
                        text: PROMPT.to_owned(),
                        attachments: Vec::new(),
                    },
                }),
                2,
            ),
        );
        // A LATER prompt with the exact same text must survive: identity is
        // the durable node/seq, never the bytes.
        deliver(
            driver,
            model,
            event(
                &EventPayload::UserMessage {
                    text: PROMPT.to_owned(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Steer,
                },
                3,
            ),
        );
        deliver(driver, model, raw(4, retraction_fact(), true));
    };

    let (mut driver, mut model) = accepted_turn();
    // Rebuild from an empty projection so this is a genuine replay, not the
    // live model the fixture already moved.
    let mut replay = launcher_model();
    replay.mode = RuntimeMode::Live;
    replay.sessions.clear();
    let mut replay_driver = LiveDriver::new("escretract-replay");
    replay_driver.apply(
        &mut replay,
        LiveReply::Listed {
            sessions: vec![summary()],
            next_cursor: None,
        },
    );
    replay_driver.ensure_attached(&replay, &sid());
    replay_driver.apply(
        &mut replay,
        LiveReply::Attached {
            session: sid(),
            attachment: attachment_id(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );
    replay.open_session(&sid());
    stream(&mut replay, &mut replay_driver);

    assert_eq!(
        user_rows(&replay),
        [PROMPT],
        "exactly one row survives — the retracted one is hidden, its equal-text \
         neighbour is not"
    );
    assert_eq!(
        replay
            .prompt_history
            .iter()
            .filter(|entry| entry.seq == Some(PROMPT_SEQ))
            .count(),
        0,
        "the retracted prompt is not offered by the Esc-Esc chooser"
    );
    assert_eq!(
        replay
            .prompt_history
            .iter()
            .filter(|entry| entry.seq == Some(3))
            .count(),
        1,
        "the equal-text prompt that was NOT retracted stays recallable"
    );

    // The live model reaches the same place from the same facts.
    deliver(&mut driver, &mut model, raw(4, retraction_fact(), true));
    assert!(user_rows(&model).is_empty());
    assert!(
        model
            .prompt_history
            .iter()
            .all(|entry| entry.seq != Some(PROMPT_SEQ))
    );
}

#[test]
fn a_receipt_for_a_session_the_user_left_parks_the_prompt_instead_of_losing_it() {
    // By the time the receipt lands the daemon has already committed the
    // retraction: the transcript row is gone and so is the history entry. If a
    // surface switch inside that one round trip made the client DROP the
    // draft, the user's text would have no remaining copy anywhere in the UI.
    // It is parked on the session it belongs to — exactly where a surface
    // switch would have left it — and is waiting on return.
    //
    // MUTATION CHECK: restore the old `active_session != session -> return`
    // early exit in the `PromptRetracted` arm and the parked-draft assertions
    // below fail.
    let (mut driver, mut model) = accepted_turn();
    let ui_gen = model
        .sessions
        .iter()
        .find(|row| row.id == sid())
        .expect("the session is on the roster")
        .ui_gen;
    let commands = esc(&mut driver, &mut model);
    let Some(LiveCommand::Retract { command_id, .. }) = commands.first().cloned() else {
        panic!("expected a retract, got {commands:?}");
    };
    // The user steps to the launcher inside the round trip.
    model.screen = Screen::Launcher;
    model.active_session = None;

    driver.apply(
        &mut model,
        LiveReply::PromptRetracted {
            command_id,
            session: sid(),
            run_id: RunId::new(RUN),
            text: PROMPT.to_owned(),
            attachments: vec![image_block()],
        },
    );
    assert!(
        model.composer.is_empty(),
        "the surface the user is looking at is never seeded behind their back"
    );
    let parked = model
        .drafts
        .get(&DraftKey::Session(ui_gen))
        .expect("the prompt is parked on its own session");
    assert_eq!(
        parked.text(),
        PROMPT,
        "the exact prompt survives the switch"
    );
    assert_eq!(
        parked.attachments().len(),
        1,
        "and so do its attachment chips"
    );
    assert!(
        !model.retract_pending,
        "the guard lifts — the user must not be stranded"
    );
}

#[test]
fn a_daemon_that_drops_the_retract_frame_still_gets_one_cancel() {
    // `turn.retract` is feature-gated in the link. A gate that answered
    // NOTHING would leave the durable command in the outbox forever — resent
    // and re-dropped on every reconnect — while Esc appeared to do nothing at
    // all. The typed failure must reach the fallback.
    //
    // MUTATION CHECK: remove `LiveCommand::Retract` from the feature_missing
    // allowlist in link.rs and the real client drops this frame silently; this
    // test pins the driver half of that contract.
    let (mut driver, mut model) = accepted_turn();
    let commands = esc(&mut driver, &mut model);
    let Some(LiveCommand::Retract { command_id, .. }) = commands.first().cloned() else {
        panic!("expected a retract, got {commands:?}");
    };
    assert_eq!(
        haider_tui::link::command_required_features(&commands[0]),
        &[haider_rpc::FEATURE_TURN_RETRACT_V1],
        "the command declares the gate that can reject it"
    );

    let fallback = driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: "feature_missing".to_owned(),
            message: "connected daemon does not advertise turn_retract_v1".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(
        matches!(fallback.as_slice(), [LiveCommand::Cancel { run_id, .. }]
            if *run_id == RunId::new(RUN)),
        "a rejected retract still stops the run, got {fallback:?}"
    );
    assert!(!model.retract_pending);
    assert_eq!(user_rows(&model), [PROMPT]);
}

#[test]
fn esc_with_no_live_run_lifts_the_guard_instead_of_stranding_the_composer() {
    // The driver refuses to invent a run id (the Interrupt law). If it issues
    // nothing, nothing will ever answer — so the guard has to lift right here
    // or Enter stays refused forever.
    //
    // MUTATION CHECK: delete the `retract_settled()` calls from the
    // `RetractPrompt` arm and `retract_pending` stays true with no command in
    // flight.
    let (mut driver, mut model) = accepted_turn();
    deliver(
        &mut driver,
        &mut model,
        event(&EventPayload::RunState(RunState::Done), 4),
    );
    model.turn_active = true; // the reducer's own flag, deliberately forced
    model.requests.clear();

    let commands = esc(&mut driver, &mut model);
    assert!(
        commands.is_empty(),
        "no run means no command — never an invented run id, got {commands:?}"
    );
    assert!(
        !model.retract_pending,
        "and the composer is not left permanently unable to send"
    );
}

#[test]
fn a_demo_turn_never_asks_for_a_retraction_it_would_have_to_invent() {
    // The demo twin has no journal, so there is no accepted draft for a daemon
    // to hand back. Esc there stays the local interrupt it has always been.
    let mut model = launcher_model();
    for character in "walk me through the harness".chars() {
        model.handle(key(KeyCode::Char(character)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(model.turn_active, "the demo turn is playing");
    assert!(
        !model.can_retract_prompt(),
        "a fabricating mode has no durable prompt to retract"
    );
    model.requests.clear();

    model.handle(key(KeyCode::Esc));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::Interrupt { .. })),
        "demo Esc stays a plain interrupt, got {:?}",
        model.requests
    );
    assert!(!model.retract_pending);
}

#[test]
fn an_older_daemon_that_cannot_retract_gets_the_cancel_it_understands() {
    // `turn.retract` is feature-gated on the wire. Sending it to a daemon that
    // never advertised `turn_retract_v1` is a frame it can only reject, which
    // the user reads as "Esc did nothing".
    let (mut driver, mut model) = accepted_turn();
    model.daemon_features.clear();
    assert!(!model.can_retract_prompt());

    let commands = esc(&mut driver, &mut model);
    assert!(
        matches!(commands.as_slice(), [LiveCommand::Cancel { .. }]),
        "Esc falls back to the cancel every daemon serves, got {commands:?}"
    );
    assert_eq!(
        haider_tui::link::command_required_features(&LiveCommand::Retract {
            command_id: CommandId::new("c"),
            session: sid(),
            worker_generation: 7,
            run_id: RunId::new(RUN),
            branch: None,
        }),
        &[haider_rpc::FEATURE_TURN_RETRACT_V1],
        "and the command declares the gate it needs"
    );
}

#[test]
fn a_parked_session_records_the_boundary_and_forgets_its_retracted_prompt() {
    // `SessionState` is the BACKGROUND twin of the attached reducer, and it
    // already mirrors the other `render.ui == false` command-state hooks for
    // this exact reason. Without the retraction pair it drifts:
    //
    // * a first response that lands while the session is parked would not
    //   close its window, so checking the session back in would offer a
    //   retraction the daemon has already refused — a spurious "too late"
    //   round trip in the user's face;
    // * a retraction that lands while parked would leave the prompt in THAT
    //   session's chooser for good, because nothing re-delivers the fact.
    //
    // MUTATION CHECK: delete the escretract hook from `SessionState::absorb_raw`
    // and both halves below fail.
    use haider_tui::session::SessionState;

    let mut parked = SessionState::neutral(sid(), haider_tui::identity::UiGeneration::new(1));
    assert!(
        !parked.projection.response_started(),
        "a fresh session has an open window"
    );

    parked.absorb_raw(&event(
        &EventPayload::UserMessage {
            text: PROMPT.to_owned(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Steer,
        },
        PROMPT_SEQ,
    ));
    parked.absorb_raw(&event(
        &EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(NODE),
            parent: None,
            kind: NodeKind::UserTurn {
                text: PROMPT.to_owned(),
                attachments: Vec::new(),
            },
        }),
        2,
    ));
    assert_eq!(
        parked
            .prompt_history
            .iter()
            .filter(|entry| entry.seq == Some(PROMPT_SEQ))
            .count(),
        1,
        "the parked session recalls its own prompt"
    );

    // The `ui == false` boundary still teaches the parked twin.
    parked.absorb_raw(&boundary(3));
    assert!(
        parked.projection.response_started(),
        "a response that arrives while parked still closes that session's window"
    );

    // And its retraction still prunes that session's own chooser.
    parked.absorb_raw(&raw(4, retraction_fact(), true));
    assert_eq!(
        parked
            .prompt_history
            .iter()
            .filter(|entry| entry.seq == Some(PROMPT_SEQ))
            .count(),
        0,
        "nothing re-delivers this fact, so the parked twin must apply it now"
    );
}
