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
use haider_protocol::error::{ErrorAction, ErrorCode};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{FakeProvider, FakeStep, ProviderErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Barrier, Semaphore};

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

/// A fact-driven backoff gate. Tests release a permit to model natural timer
/// completion; if a provider request appears without one, the manual wake was
/// the only path that could have advanced the retry ladder.
#[derive(Debug)]
struct GatedSleeper {
    delays: Mutex<Vec<u64>>,
    starts: AtomicUsize,
    permits: Semaphore,
}

impl Default for GatedSleeper {
    fn default() -> Self {
        Self {
            delays: Mutex::new(Vec::new()),
            starts: AtomicUsize::new(0),
            permits: Semaphore::new(0),
        }
    }
}

impl GatedSleeper {
    fn recorded(&self) -> Vec<u64> {
        self.delays.lock().expect("sleeper lock").clone()
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn complete_one_naturally(&self) {
        self.permits.add_permits(1);
    }
}

#[async_trait]
impl RetrySleeper for GatedSleeper {
    async fn sleep(&self, delay_ms: u64) {
        self.delays.lock().expect("sleeper lock").push(delay_ms);
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.permits
            .acquire()
            .await
            .expect("test sleeper remains open")
            .forget();
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
    serde_json::from_value(envelope.payload.clone().into()).expect("known payload")
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

fn provider_waiting_states(events: &[RawEnvelope]) -> Vec<RunState> {
    events
        .iter()
        .filter_map(|envelope| match typed(envelope) {
            EventPayload::RunState(
                state @ RunState::Waiting {
                    reason: WaitReason::RateLimit | WaitReason::ProviderBackoff,
                },
            ) => Some(state),
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

async fn wait_for_retrying_event(store: &MemoryStore) -> RawEnvelope {
    wait_for_retrying_event_after(store, 0).await
}

async fn wait_for_retrying_event_after(store: &MemoryStore, after_seq: u64) -> RawEnvelope {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(event) = store
                .events(&SessionId::new(SESSION))
                .await
                .into_iter()
                .find(|event| {
                    event.seq > after_seq
                        && matches!(
                            typed(event),
                            EventPayload::RunState(RunState::Retrying { .. })
                        )
                })
            {
                return event;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Retrying fact appears on the poll grid")
}

async fn wait_for_request_count(provider: &FakeProvider, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if provider.requests().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request fact appears on the poll grid");
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
    assert_eq!(
        provider_waiting_states(&events),
        vec![RunState::Waiting {
            reason: WaitReason::ProviderBackoff,
        }],
        "the grader-visible provider_backoff telemetry precedes retry"
    );
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

/// MUTATION CHECK: remove the wake branch, bind it to the wrong event id, or
/// increment the attempt on wake. Expected runtime failure: with no natural
/// permit released the second provider request never appears, or the durable
/// retry fact ceases to say that the next request is attempt 2.
#[tokio::test]
async fn m4_manual_wake_fires_next_attempt_without_natural_backoff_completion() {
    let sleeper = Arc::new(GatedSleeper::default());
    let (handle, store, provider) = spawn(
        vec![
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "wake this backoff".into(),
                retry_after_ms: Some(60_000),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("retry now"))
        .await
        .expect("turn accepted");
    let retrying_event = wait_for_retrying_event(&store).await;

    assert_eq!(provider.requests().len(), 1, "the backoff is still gated");
    assert_eq!(sleeper.starts(), 1, "one natural wait is pending");
    assert_eq!(sleeper.recorded(), vec![60_000]);
    assert!(matches!(
        typed(&retrying_event),
        EventPayload::RunState(RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: 60_000,
            reason: WaitReason::ProviderBackoff,
        })
    ));
    assert!(
        handle.wake_provider_retry("manual-wake", &retrying_event.event_id),
        "the exact armed durable fact accepts one wake"
    );

    // No sleeper permit is released: request #2 is proof that the wake, not
    // natural timer completion, advanced the existing attempt ladder.
    wait_for_request_count(&provider, 2).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("woken turn completes")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(
        retrying_states(&store.events(&SessionId::new(SESSION)).await).len(),
        1,
        "manual wake does not mint or renumber a retry fact"
    );
}

/// MUTATION CHECK: make the latch edge-triggered without retained state, omit
/// the fired predicate, or key coalescing only by command id. Expected runtime
/// failure: an early wake is lost, or duplicate/distinct wakes are reported as
/// additional winners and can advance more than one provider attempt.
#[tokio::test]
async fn m4_duplicate_and_distinct_wakes_coalesce_to_one_attempt() {
    let sleeper = Arc::new(GatedSleeper::default());
    let (handle, store, provider) = spawn(
        vec![
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "coalesce wakes".into(),
                retry_after_ms: Some(60_000),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper,
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("wake twice"))
        .await
        .expect("turn accepted");
    let retrying_event = wait_for_retrying_event(&store).await;

    assert!(handle.wake_provider_retry("same-command", &retrying_event.event_id));
    assert!(
        !handle.wake_provider_retry("same-command", &retrying_event.event_id),
        "receipt replay is an idempotent no-op"
    );
    assert!(
        !handle.wake_provider_retry("another-command", &retrying_event.event_id),
        "N manual clicks still short-circuit this backoff only once"
    );
    wait_for_request_count(&provider, 2).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("turn completes after coalesced wake")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2, "only the next attempt fires");
}

/// MUTATION CHECK: remove exact event-id matching, allow a stale command to
/// rebind to a later event, or let an earlier notification resolve a later
/// armed wait. Expected runtime failure: the old event wakes attempt 3, its
/// consumed command wakes the new event, or the second request advances
/// without its own exact wake.
#[tokio::test]
async fn m4_old_event_cannot_wake_the_next_backoff_in_the_same_run() {
    let sleeper = Arc::new(GatedSleeper::default());
    let (handle, store, provider) = spawn(
        vec![
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "first backoff".into(),
                retry_after_ms: Some(60_000),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::Overloaded,
                message: "second backoff".into(),
                retry_after_ms: Some(60_000),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("two backoffs"))
        .await
        .expect("turn accepted");
    let first_retrying = wait_for_retrying_event(&store).await;
    assert!(handle.wake_provider_retry("wake-first", &first_retrying.event_id));
    wait_for_request_count(&provider, 2).await;

    let second_retrying = wait_for_retrying_event_after(&store, first_retrying.seq).await;
    assert_ne!(first_retrying.event_id, second_retrying.event_id);
    assert_eq!(sleeper.starts(), 2, "the second natural wait is armed");
    assert!(matches!(
        typed(&second_retrying),
        EventPayload::RunState(RunState::Retrying {
            attempt: 3,
            max: 10,
            delay_ms: 60_000,
            reason: WaitReason::ProviderBackoff,
        })
    ));
    assert!(
        !handle.wake_provider_retry("fresh-command-old-event", &first_retrying.event_id),
        "an old receipt coordinate cannot wake a later backoff"
    );
    assert!(
        !handle.wake_provider_retry("fresh-command-old-event", &second_retrying.event_id),
        "a command consumed with stale coordinates cannot rebind to a later event"
    );
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        provider.requests().len(),
        2,
        "neither the old event nor an earlier notification advances attempt 3"
    );
    assert!(
        handle.wake_provider_retry("wake-second", &second_retrying.event_id),
        "the exact current event remains wakeable after the stale request"
    );
    wait_for_request_count(&provider, 3).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("twice-woken turn completes")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 3);
}

/// MUTATION CHECK: let natural completion and wake independently schedule a
/// retry, or disarm after an await. Expected runtime failure: at least one
/// barrier race iteration records a third provider request.
#[tokio::test]
async fn m4_natural_backoff_completion_racing_wake_never_double_fires() {
    for iteration in 0..32 {
        let sleeper = Arc::new(GatedSleeper::default());
        let (handle, store, provider) = spawn(
            vec![
                FakeStep::Error {
                    kind: ProviderErrorKind::Overloaded,
                    message: format!("race {iteration}"),
                    retry_after_ms: Some(60_000),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
            sleeper.clone(),
        );
        let turn = handle
            .submit_turn(SubmitTurn::new(format!("race {iteration}")))
            .await
            .expect("turn accepted");
        let retrying_event = wait_for_retrying_event(&store).await;
        let barrier = Arc::new(Barrier::new(3));
        let natural_barrier = Arc::clone(&barrier);
        let natural_sleeper = Arc::clone(&sleeper);
        let natural = tokio::spawn(async move {
            natural_barrier.wait().await;
            natural_sleeper.complete_one_naturally();
        });
        let wake_barrier = Arc::clone(&barrier);
        let wake_handle = handle.clone();
        let retrying_event_id = retrying_event.event_id.clone();
        let wake = tokio::spawn(async move {
            wake_barrier.wait().await;
            wake_handle.wake_provider_retry(format!("race-wake-{iteration}"), &retrying_event_id)
        });
        barrier.wait().await;
        natural.await.expect("natural racer joins");
        let _wake_won = wake.await.expect("wake racer joins");

        wait_for_request_count(&provider, 2).await;
        let outcome = tokio::time::timeout(Duration::from_secs(5), turn.wait())
            .await
            .expect("raced turn completes")
            .expect("turn outcome");
        assert_eq!(outcome.state, RunState::Done);
        assert_eq!(
            provider.requests().len(),
            2,
            "race iteration {iteration} fires exactly the existing next attempt"
        );
    }
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
    assert!(
        !handle.wake_provider_retry("cannot-resurrect", &EventId::new("not-armed")),
        "a non-retryable terminal has no wakeable backoff"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "a forged wake cannot resurrect the terminal run"
    );
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
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(
        provider_waiting_states(&events),
        vec![RunState::Waiting {
            reason: WaitReason::RateLimit,
        }],
        "the grader-visible rate_limit telemetry precedes retry"
    );
    let retrying = retrying_states(&events);
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

/// A deadline that elapses while the durable run is in provider backoff is
/// retry exhaustion even if the actor next observes it at request admission.
/// The unopened second provider future proves this classification does not
/// depend on the backoff timer and request-open cutoff racing to wake first.
#[tokio::test(start_paused = true)]
async fn m4_deadline_expiry_in_backoff_is_bounded_provider_failure() {
    let sleeper = Arc::new(GatedSleeper::default());
    let mut config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("m4-backoff-deadline"),
        1,
        1,
    );
    config.retry_sleeper = sleeper.clone();
    // Registry #94: 500ms Retry-After + two 1s provider margins = 2.5s,
    // leaving 500ms inside this explicit 3s run deadline for admission.
    config.provider_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(3));
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::RateLimited,
            message: "wait beyond the run deadline".into(),
            retry_after_ms: Some(500),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("deadline during backoff"))
        .await
        .expect("turn accepted");

    let retrying = wait_for_retrying_event(&store).await;
    assert!(matches!(
        typed(&retrying),
        EventPayload::RunState(RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: 500,
            reason: WaitReason::RateLimit,
        })
    ));
    tokio::time::advance(Duration::from_secs(4)).await;
    sleeper.complete_one_naturally();

    let outcome = tokio::time::timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("state-classified terminal is prompt")
        .expect("turn outcome");
    let error = outcome.error.expect("typed terminal error");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(!error.retryable);
    assert_eq!(
        error
            .presentation
            .expect("bounded provider failure presentation")
            .allowed_actions,
        vec![ErrorAction::None]
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the expired retry is classified before opening request two"
    );
    assert_eq!(
        terminal_count(&store.events(&SessionId::new(SESSION)).await),
        1,
        "the state-derived path commits exactly one terminal"
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

/// A failure AFTER content was committed for the turn is NEVER auto-retried.
/// It parks for an explicit partial-stream choice; only that user choice may
/// issue another request.
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
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("content then fail"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor remains available")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("input state") else {
        panic!("expected partial-stream menu");
    };

    assert_eq!(
        provider.requests().len(),
        1,
        "a committed-content failure does not re-issue before the user chooses"
    );
    assert!(
        retrying_states(&store.events(&SessionId::new(SESSION)).await).is_empty(),
        "no Retrying beat once content has streamed"
    );
    assert!(sleeper.recorded().is_empty());
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: Some("retry_fresh".into()),
            option_index: 1,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("retry-fresh answer");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);
    assert_eq!(
        provider.requests().len(),
        2,
        "the second request follows the explicit user choice"
    );
}
