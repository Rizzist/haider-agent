//! v0.0.936 #25 delta-coalescing law pins. Provider-stream deltas may sit in
//! memory for at most the configured window (owner default 50ms) and must
//! flush at every semantic boundary; `Completed` items stay authoritative,
//! and a `Duration::ZERO` window restores the envelope-per-delta cadence.

#![allow(clippy::expect_used)]

use haider_core::{
    HarnessActor, HarnessConfig, HarnessHandle, MemoryStore, STREAM_DELTA_COALESCE_WINDOW,
    SubmitTurn,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_provider::{FakeProvider, FakeStep};
use std::sync::Arc;
use std::time::Duration;

const SESSION: &str = "session-delta-coalescing";

fn runtime_with_window(
    script: Vec<FakeStep>,
    window: Duration,
) -> (HarnessHandle, Arc<MemoryStore>) {
    let config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("device-delta"),
        17,
        23,
    )
    .with_stream_delta_coalesce_window(window);
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider, store.clone());
    (handle, store)
}

async fn journal_for(script: Vec<FakeStep>, window: Duration) -> Vec<RawEnvelope> {
    let (handle, store) = runtime_with_window(script, window);
    let turn = handle
        .submit_turn(SubmitTurn::new("coalesce me"))
        .await
        .expect("turn accepted");
    turn.wait().await.expect("turn outcome");
    store.events(&SessionId::new(SESSION)).await
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known payload")
}

/// Every journaled delta in seq order, tagged by variant.
fn delta_payloads(events: &[RawEnvelope]) -> Vec<(&'static str, String)> {
    events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Delta { delta, .. }) => Some(match delta {
                ItemDelta::Text { text } => ("text", text),
                ItemDelta::Reasoning { text } => ("reasoning", text),
                ItemDelta::ToolArgs { fragment } => ("tool_args", fragment),
                _ => ("other", String::new()),
            }),
            _ => None,
        })
        .collect()
}

fn completed_message(events: &[RawEnvelope]) -> String {
    events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .expect("completed agent message")
        .into_string()
}

fn seq_of(events: &[RawEnvelope], want: impl Fn(&EventPayload) -> bool) -> u64 {
    events
        .iter()
        .find(|event| want(&typed(event)))
        .map(|event| event.seq)
        .expect("matching envelope")
}

/// The policy value is a law, not an implementation detail: pin the literal
/// so a silent window change trips a review, not a benchmark regression.
#[test]
fn coalesce_window_literal_is_fifty_milliseconds() {
    assert_eq!(STREAM_DELTA_COALESCE_WINDOW, Duration::from_millis(50));
}

/// MUTATION CHECK: making `merge_contiguous_item_delta` return `false`, or
/// deleting the buffering arm in `commit_item` (envelope-per-delta returns),
/// must fail the single-envelope count below.
#[tokio::test]
async fn contiguous_text_deltas_coalesce_into_one_envelope_before_completion() {
    let events = journal_for(
        vec![
            FakeStep::EmitText {
                text: "alpha ".into(),
            },
            FakeStep::EmitText {
                text: "beta".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        STREAM_DELTA_COALESCE_WINDOW,
    )
    .await;
    assert_eq!(
        delta_payloads(&events),
        vec![("text", "alpha beta".to_owned())],
        "back-to-back same-item text deltas journal as one merged envelope"
    );
    assert_eq!(completed_message(&events), "alpha beta");
    let delta_seq = seq_of(&events, |payload| {
        matches!(payload, EventPayload::Item(ItemEvent::Delta { .. }))
    });
    let completed_seq = seq_of(&events, |payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { .. },
                ..
            })
        )
    });
    assert!(
        delta_seq < completed_seq,
        "the buffered delta must flush before its item completes"
    );
}

/// MUTATION CHECK: deleting the `delta_flush_timer` select branch must fail
/// this pin — without the timer, "early" survives the 300ms silence in the
/// buffer and merges with "late" into a single envelope.
#[tokio::test]
async fn window_elapse_flushes_a_buffered_delta_without_a_semantic_boundary() {
    let events = journal_for(
        vec![
            FakeStep::EmitText {
                text: "early".into(),
            },
            FakeStep::Delay { ms: 300 },
            FakeStep::EmitText {
                text: "late".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        STREAM_DELTA_COALESCE_WINDOW,
    )
    .await;
    let deltas = delta_payloads(&events);
    assert_eq!(
        deltas,
        vec![("text", "early".to_owned()), ("text", "late".to_owned()),],
        "the timed flush must commit the first delta during provider silence"
    );
    // Mixed-granularity parity seam: the coarse envelopes concatenate to the
    // authoritative completed text.
    assert_eq!(completed_message(&events), "earlylate");
}

/// MUTATION CHECK: neutering `delta_flush_timer` (never firing) must fail
/// this pin. The journal-shape pins cannot catch that mutation — the
/// buffer-entry deadline check flushes once a NEXT delta arrives — so this
/// pin observes the timer's distinct job: publishing during provider
/// silence, long before the next semantic boundary. The 500ms bound is 10x
/// the 50ms window (gate-load doctrine: wall-clock pins need wide margins).
#[tokio::test]
async fn timed_flush_publishes_the_delta_during_provider_silence() {
    let (handle, _store) = runtime_with_window(
        vec![
            FakeStep::EmitText {
                text: "early".into(),
            },
            FakeStep::Delay { ms: 700 },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        STREAM_DELTA_COALESCE_WINDOW,
    );
    let mut subscriber = handle.subscribe();
    let started = tokio::time::Instant::now();
    let turn = handle
        .submit_turn(SubmitTurn::new("coalesce me"))
        .await
        .expect("turn accepted");
    let waited = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let event = subscriber.recv().await.expect("event stream open");
            if matches!(typed(&event), EventPayload::Item(ItemEvent::Delta { .. })) {
                break;
            }
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "the timed flush must publish the buffered delta during the provider's \
         700ms silence, not at the next semantic boundary (waited {}ms)",
        started.elapsed().as_millis()
    );
    turn.wait().await.expect("turn outcome");
}

/// MUTATION CHECK: replacing the buffer on an interleave without flushing
/// (dropping the buffered delta) must fail the ordered three-envelope pin.
#[tokio::test]
async fn interleaved_delta_kinds_flush_as_ordered_separate_envelopes() {
    let events = journal_for(
        vec![
            FakeStep::EmitText { text: "t1".into() },
            FakeStep::EmitReasoning { text: "r1".into() },
            FakeStep::EmitText { text: "t2".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        STREAM_DELTA_COALESCE_WINDOW,
    )
    .await;
    assert_eq!(
        delta_payloads(&events),
        vec![
            ("text", "t1".to_owned()),
            ("reasoning", "r1".to_owned()),
            ("text", "t2".to_owned()),
        ],
        "each kind boundary flushes the prior delta, order preserved"
    );
}

/// MUTATION CHECK: ignoring `stream_delta_coalesce_window` (hard-coding the
/// default) must fail this pin — under a zero window the two deltas merge
/// into one envelope and the two-envelope cadence assertion breaks.
#[tokio::test]
async fn zero_window_restores_the_envelope_per_delta_cadence() {
    let events = journal_for(
        vec![
            FakeStep::EmitText { text: "a".into() },
            FakeStep::EmitText { text: "b".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        Duration::ZERO,
    )
    .await;
    assert_eq!(
        delta_payloads(&events),
        vec![("text", "a".to_owned()), ("text", "b".to_owned())],
        "Duration::ZERO must restore one durable envelope per provider delta"
    );
    assert_eq!(completed_message(&events), "ab");
}
