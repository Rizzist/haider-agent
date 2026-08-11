#![allow(clippy::expect_used)]
//! W-C M4 — Claude-Code-style API-error retry with a visible attempt counter.
//!
//! Laws:
//! - a retryable pre-first-event failure re-issues the request under a
//!   `Retrying { attempt, max, delay_ms, reason }` beat, and the counter shown
//!   is the NEXT attempt (a first failure renders `attempt 2`);
//! - a non-retryable error (400/401) is immediate `Errored`, no `Retrying`,
//!   no wait;
//! - a present `retry_after_ms` OVERRIDES the computed backoff;
//! - exhausted retries latch `Errored` exactly once;
//! - the backoff schedule is a PURE function of the attempt (assert the
//!   sequence);
//! - a failure AFTER content was committed is never retried (no duplicate).
//!
//! The injected [`RecordingSleeper`] returns immediately and records every
//! requested backoff, so the schedule is asserted without any wall-clock wait.

use async_trait::async_trait;
use haider_core::{
    HarnessActor, HarnessConfig, HarnessHandle, MemoryStore, RetrySleeper, SubmitTurn,
    retry_backoff_ms, retry_jittered_backoff_ms,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{FakeProvider, FakeStep, ProviderErrorKind};
use std::sync::{Arc, Mutex};

const SESSION: &str = "m4-session";

/// An instant [`RetrySleeper`] that records every requested backoff.
#[derive(Debug, Default)]
struct RecordingSleeper {
    delays: Mutex<Vec<u64>>,
}

impl RecordingSleeper {
    fn recorded(&self) -> Vec<u64> {
        self.delays.lock().expect("sleeper lock").clone()
    }
}

#[async_trait]
impl RetrySleeper for RecordingSleeper {
    async fn sleep(&self, delay_ms: u64) {
        self.delays.lock().expect("sleeper lock").push(delay_ms);
    }
}

fn spawn(
    script: Vec<FakeStep>,
    sleeper: Arc<dyn RetrySleeper>,
) -> (HarnessHandle, Arc<MemoryStore>, Arc<FakeProvider>) {
    let mut config = HarnessConfig::for_session(SessionId::new(SESSION), DeviceId::new("m4"), 1, 1);
    config.retry_sleeper = sleeper;
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    (handle, store, provider)
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known payload")
}

fn retrying_states(events: &[RawEnvelope]) -> Vec<RunState> {
    events
        .iter()
        .filter_map(|envelope| match typed(envelope) {
            EventPayload::RunState(state @ RunState::Retrying { .. }) => Some(state),
            _ => None,
        })
        .collect()
}

fn terminal_count(events: &[RawEnvelope]) -> usize {
    events
        .iter()
        .filter(|envelope| {
            matches!(typed(envelope), EventPayload::RunState(state) if state.is_terminal())
        })
        .count()
}

/// A retryable handshake failure re-issues the request under a visible
/// `Retrying` beat, then the turn completes — the counter is the NEXT attempt.
#[tokio::test]
async fn m4_retryable_failure_retries_then_completes_with_visible_counter() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        vec![
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "overloaded".into(),
                retry_after_ms: None,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("retry me"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(
        provider.requests().len(),
        2,
        "one retry re-issued the request"
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    let run_id = events
        .iter()
        .find_map(|event| event.run_id.clone())
        .expect("run id");
    let retrying = retrying_states(&events);
    let expected_delay = retry_jittered_backoff_ms(&run_id, 1);
    assert_eq!(retrying.len(), 1, "exactly one visible retry beat");
    assert_eq!(
        retrying[0],
        RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: expected_delay,
            reason: WaitReason::ProviderBackoff,
        },
        "first failure shows the NEXT attempt (2/10) with jittered backoff"
    );
    assert_eq!(sleeper.recorded(), vec![expected_delay]);
}

/// A non-retryable error (400/401) surfaces immediately: no `Retrying`, no
/// wait.
#[tokio::test]
async fn m4_non_retryable_error_is_immediate_errored_without_retrying() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        vec![FakeStep::Error {
            kind: ProviderErrorKind::InvalidRequest,
            message: "invalid request".into(),
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("bad request"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert!(
        !outcome.error.expect("typed error").retryable,
        "a 400 is not retryable"
    );
    assert_eq!(provider.requests().len(), 1, "no re-issue");
    assert!(
        retrying_states(&store.events(&SessionId::new(SESSION)).await).is_empty(),
        "a non-retryable error never commits a Retrying beat"
    );
    assert!(sleeper.recorded().is_empty(), "no backoff was waited");
}

/// A provider `retry_after_ms` (429/529 Retry-After) OVERRIDES the computed
/// exponential backoff.
#[tokio::test]
async fn m4_retry_after_overrides_computed_backoff() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, _provider) = spawn(
        vec![
            FakeStep::Error {
                kind: ProviderErrorKind::RateLimited,
                message: "rate limited".into(),
                retry_after_ms: Some(7_000),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("respect retry-after"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(outcome.state, RunState::Done);
    // The server's 7s instruction is NOT the computed 1s first backoff.
    assert_ne!(retry_backoff_ms(1), 7_000);
    assert_eq!(
        sleeper.recorded(),
        vec![7_000],
        "the server's Retry-After won"
    );
    let retrying = retrying_states(&store.events(&SessionId::new(SESSION)).await);
    assert_eq!(
        retrying[0],
        RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: 7_000,
            reason: WaitReason::RateLimit,
        }
    );
}

/// Ten consecutive retryable failures exhaust the ceiling and latch `Errored`
/// exactly once, after nine visible retry beats.
#[tokio::test]
async fn m4_exhausted_retries_latch_errored_once() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let script: Vec<FakeStep> = (0..10)
        .map(|_| FakeStep::Error {
            kind: ProviderErrorKind::Overloaded,
            message: "still overloaded".into(),
            retry_after_ms: None,
        })
        .collect();
    let (handle, store, provider) = spawn(script, sleeper.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("never recovers"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert!(
        outcome.error.expect("typed error").retryable,
        "exhaustion preserves the retryable classification"
    );
    assert_eq!(
        provider.requests().len(),
        10,
        "MAX_API_RETRIES attempts total"
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    let run_id = events
        .iter()
        .find_map(|event| event.run_id.clone())
        .expect("run id");
    assert_eq!(
        retrying_states(&events).len(),
        9,
        "nine retry beats (attempt 2..10) precede the terminal"
    );
    assert_eq!(terminal_count(&events), 1, "exactly one Errored terminal");
    assert_eq!(
        sleeper.recorded(),
        (1..=9)
            .map(|attempt| retry_jittered_backoff_ms(&run_id, attempt))
            .collect::<Vec<_>>(),
        "the exact run-scoped jitter sequence was waited"
    );
}

/// The base remains pure and capped; run-scoped jitter is stable and stays in
/// the lower half of each window.
#[test]
fn m4_backoff_schedule_is_a_pure_function_of_attempt() {
    let sequence: Vec<u64> = (1..=8).map(retry_backoff_ms).collect();
    assert_eq!(
        sequence,
        vec![1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000],
        "exponential doubling, capped at 30s from the sixth attempt"
    );
    // Purity: the same attempt always yields the same delay.
    assert_eq!(retry_backoff_ms(3), retry_backoff_ms(3));
    // Attempt 0 saturates to the base instead of underflowing.
    assert_eq!(retry_backoff_ms(0), 1_000);
    let run = haider_protocol::ids::RunId::new("m4-jitter");
    for attempt in 1..=8 {
        let base = retry_backoff_ms(attempt);
        let jittered = retry_jittered_backoff_ms(&run, attempt);
        assert!((base / 2..=base).contains(&jittered));
        assert_eq!(jittered, retry_jittered_backoff_ms(&run, attempt));
    }
}

/// A failure AFTER content was committed for the turn is NEVER auto-retried —
/// re-issuing would duplicate the already-streamed output.
#[tokio::test]
async fn m4_failure_after_committed_content_is_not_retried() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        vec![
            FakeStep::EmitText {
                text: "already streamed".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "broke mid-stream".into(),
                retry_after_ms: None,
            },
        ],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("content then fail"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        provider.requests().len(),
        1,
        "a committed-content failure never re-issues"
    );
    assert!(
        retrying_states(&store.events(&SessionId::new(SESSION)).await).is_empty(),
        "no Retrying beat once content has streamed"
    );
    assert!(sleeper.recorded().is_empty());
}
