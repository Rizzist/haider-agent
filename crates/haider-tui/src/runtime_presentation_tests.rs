#![allow(clippy::expect_used)]

use super::*;
use crate::live::LiveReply;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_rpc::AttachmentId;

fn payload_envelope(
    session: &SessionId,
    run: &RunId,
    seq: u64,
    payload: EventPayload,
) -> Box<haider_protocol::envelope::RawEnvelope> {
    Box::new(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("presentation-event-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: Some(run.clone()),
        agent_id: None,
        device_id: DeviceId::new("presentation-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(payload)
            .expect("presentation payload serializes")
            .into(),
    })
}

fn event(
    attachment: &AttachmentId,
    session: &SessionId,
    envelope: Box<haider_protocol::envelope::RawEnvelope>,
) -> LiveReply {
    LiveReply::Event {
        attachment: attachment.clone(),
        session: session.clone(),
        envelope,
    }
}

fn started(item: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(item),
        item: TurnItem::AgentMessage {
            text: String::new().into(),
        },
    })
}

fn delta(item: &str, text: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new(item),
        delta: ItemDelta::Text {
            text: text.to_owned().into(),
        },
    })
}

fn extension(item: &str, kind: &str, data: serde_json::Value) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(item),
        item: TurnItem::Extension {
            kind: kind.to_owned(),
            data,
        },
    })
}

#[test]
fn live_presentation_gate_accelerates_only_live_first_content_and_terminal_edges() {
    let session = SessionId::new("presentation-session");
    let background = SessionId::new("background-session");
    let attachment = AttachmentId::new("presentation-attachment");
    let run = RunId::new("presentation-run");
    let mut gate = LivePresentationGate::default();

    assert_eq!(
        gate.observe(
            &LiveReply::Attached {
                session: session.clone(),
                attachment: attachment.clone(),
                worker_generation: 1,
                replay_through_seq: 3,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "attaching is not itself a draw edge",
    );

    for (seq, payload) in [
        (1, EventPayload::RunState(RunState::Thinking)),
        (2, started("item-1")),
        (3, delta("item-1", "replayed content")),
    ] {
        assert_eq!(
            gate.observe(
                &event(
                    &attachment,
                    &session,
                    payload_envelope(&session, &run, seq, payload),
                ),
                Some(&session),
            ),
            ImmediatePresentation::default(),
            "historical replay never accelerates seq {seq}",
        );
    }
    assert_eq!(
        gate.observe(
            &LiveReply::CaughtUp {
                attachment: attachment.clone(),
                high_water_seq: 3,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run, 4, delta("item-1", " continuing")),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "content reconstructed during replay does not re-arm on a later delta",
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run, 5, EventPayload::RunState(RunState::Done),),
            ),
            Some(&session),
        ),
        ImmediatePresentation {
            first_content: false,
            terminal: true,
        },
        "live terminal truth bypasses the frame wait",
    );

    let run_two = RunId::new("presentation-run-two");
    for (seq, payload) in [
        (6, EventPayload::RunState(RunState::Thinking)),
        (7, started("item-2")),
    ] {
        assert_eq!(
            gate.observe(
                &event(
                    &attachment,
                    &session,
                    payload_envelope(&session, &run_two, seq, payload),
                ),
                Some(&session),
            ),
            ImmediatePresentation::default(),
            "an empty item opening is not first content",
        );
    }
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run_two, 8, delta("item-2", "first token")),
            ),
            Some(&session),
        ),
        ImmediatePresentation {
            first_content: true,
            terminal: false,
        },
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run_two, 9, delta("item-2", " more")),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "continuing deltas retain the frame cadence",
    );

    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(
                    &session,
                    &run_two,
                    10,
                    EventPayload::RunState(RunState::Done),
                ),
            ),
            Some(&background),
        ),
        ImmediatePresentation::default(),
        "a background terminal event does not force the foreground terminal to draw",
    );
    assert!(!ImmediatePresentation::default().required());
}

#[test]
fn live_presentation_gate_rejects_missing_catchup_duplicates_and_gaps() {
    let session = SessionId::new("sequence-session");
    let attachment = AttachmentId::new("sequence-attachment");
    let run = RunId::new("sequence-run");
    let mut gate = LivePresentationGate::default();
    let attached = LiveReply::Attached {
        session: session.clone(),
        attachment: attachment.clone(),
        worker_generation: 1,
        replay_through_seq: 0,
    };
    assert_eq!(
        gate.observe(&attached, Some(&session)),
        ImmediatePresentation::default(),
    );

    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run, 1, delta("item", "early")),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "no event accelerates before the catch-up boundary",
    );
    assert_eq!(
        gate.observe(
            &LiveReply::CaughtUp {
                attachment: attachment.clone(),
                high_water_seq: 1,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run, 3, delta("item", "gap")),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "a sequence gap cannot force a draw",
    );
    let continuing = event(
        &attachment,
        &session,
        payload_envelope(&session, &run, 2, delta("item", "continuing")),
    );
    assert_eq!(
        gate.observe(&continuing, Some(&session)),
        ImmediatePresentation::default(),
        "content reconstructed before caught-up stays continuing content",
    );
    assert_eq!(
        gate.observe(&continuing, Some(&session)),
        ImmediatePresentation::default(),
        "a duplicate cannot force a draw",
    );
}

#[test]
fn live_presentation_gate_opens_after_a_raised_catchup_high_water() {
    let session = SessionId::new("raised-high-water-session");
    let attachment = AttachmentId::new("raised-high-water-attachment");
    let run = RunId::new("raised-high-water-run");
    let mut gate = LivePresentationGate::default();

    assert_eq!(
        gate.observe(
            &LiveReply::Attached {
                session: session.clone(),
                attachment: attachment.clone(),
                worker_generation: 1,
                replay_through_seq: 9,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(
                    &session,
                    &run,
                    10,
                    EventPayload::RunState(RunState::Thinking),
                ),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "the replay tail above the attach snapshot never accelerates",
    );
    assert_eq!(
        gate.observe(
            &LiveReply::CaughtUp {
                attachment: attachment.clone(),
                high_water_seq: 10,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(&session, &run, 11, delta("item", "first live token")),
            ),
            Some(&session),
        ),
        ImmediatePresentation {
            first_content: true,
            terminal: false,
        },
        "the first event after the raised catch-up boundary is live",
    );
}

#[test]
fn live_presentation_gate_matches_visible_and_hidden_extension_rows() {
    let session = SessionId::new("extension-session");
    let attachment = AttachmentId::new("extension-attachment");
    let run = RunId::new("extension-run");
    let mut gate = LivePresentationGate::default();

    assert_eq!(
        gate.observe(
            &LiveReply::Attached {
                session: session.clone(),
                attachment: attachment.clone(),
                worker_generation: 1,
                replay_through_seq: 0,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );
    assert_eq!(
        gate.observe(
            &LiveReply::CaughtUp {
                attachment: attachment.clone(),
                high_water_seq: 0,
            },
            Some(&session),
        ),
        ImmediatePresentation::default(),
    );

    let footprint = ContextFootprint {
        input_tokens: 800,
        output_tokens: 100,
        cached_input_tokens: 100,
        used_tokens: 1_000,
        context_window: Some(10_000),
        reserved_output_tokens: 1_000,
        soft_threshold_tokens: Some(8_500),
        estimated_turns_to_threshold: Some(4),
        truth: ContextFootprintTruth::Exact,
        accounting: None,
    };
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(
                    &session,
                    &run,
                    1,
                    EventPayload::Item(ItemEvent::Started {
                        item_id: ItemId::new("footprint"),
                        item: footprint.extension_item().expect("footprint serializes"),
                    }),
                ),
            ),
            Some(&session),
        ),
        ImmediatePresentation::default(),
        "context-footprint metadata never consumes first content",
    );
    assert_eq!(
        gate.observe(
            &event(
                &attachment,
                &session,
                payload_envelope(
                    &session,
                    &run,
                    2,
                    extension(
                        "visible-extension",
                        "future_visible_fact_v1",
                        serde_json::json!({ "label": "visible extension" }),
                    ),
                ),
            ),
            Some(&session),
        ),
        ImmediatePresentation {
            first_content: true,
            terminal: false,
        },
        "the renderer's generic extension row is first visible content",
    );
}
