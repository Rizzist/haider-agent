#![allow(clippy::expect_used)]
//! Paused-time bounds come directly from the idle budget and scripted waits.
use async_trait::async_trait;
use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::{CapabilityDoc, FinishReason};
use haider_protocol::state::RunState;
use haider_provider::{
    FakeProvider, FakeStep, Provider, ProviderError, ProviderErrorKind, ProviderStream,
    ProviderTimeoutReason, TurnRequest,
};
use std::sync::Arc;
use tokio::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(4);

struct IdleProvider {
    fake: FakeProvider,
    stall_open: bool,
    open_timeout: bool,
}

#[async_trait]
impl Provider for IdleProvider {
    fn idle_timeout(&self) -> Option<Duration> {
        Some(BUDGET)
    }
    async fn capabilities(&self) -> CapabilityDoc {
        self.fake.capabilities().await
    }
    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        if self.stall_open {
            std::future::pending::<()>().await;
        }
        if self.open_timeout {
            tokio::time::sleep(BUDGET / 4).await;
            return Err(ProviderError::new(
                ProviderErrorKind::Transport,
                "response open timed out",
            )
            .with_timeout_reason(ProviderTimeoutReason::ResponseOpen)
            .with_presentation(haider_protocol::error::ErrorPresentation::new(
                "provider-timeout",
                "Provider timeout",
                "Response headers did not arrive",
                haider_protocol::error::ErrorScope::Turn,
                [haider_protocol::error::ErrorAction::Retry],
            )));
        }
        self.fake.stream_turn(request).await
    }
}

async fn run(provider: Arc<IdleProvider>) -> (haider_core::TurnOutcome, Duration) {
    let config = HarnessConfig::for_session(SessionId::new("idle"), DeviceId::new("idle"), 1, 1);
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider, store.clone());
    let started = Instant::now();
    let outcome = handle
        .submit_turn(SubmitTurn::new("idle regression"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("terminal");
    let elapsed = started.elapsed();
    if let Some(error) = outcome
        .error
        .as_ref()
        .filter(|error| error.code == ErrorCode::IdleTimeout)
    {
        // SQLite writers and replay readers share this projection. The
        // actor's typed idle failure must remain a timeout on both surfaces.
        let terminal = haider_protocol::headless::durable_run_terminal_v1(
            outcome.state.clone(),
            Some(error.code),
            false,
            false,
            None,
        )
        .expect("durable terminal");
        assert_eq!(terminal.terminal_kind, "timeout");
        assert_eq!(terminal.error_code, Some("idle_timeout"));
        let events = store.events(&SessionId::new("idle")).await;
        let evidence = events
            .iter()
            .filter_map(|event| {
                match serde_json::from_value::<haider_protocol::EventPayload>(
                    event.payload.clone().into(),
                )
                .expect("typed event")
                {
                    haider_protocol::EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed {
                            item: haider_protocol::item::TurnItem::Extension { kind, data },
                            ..
                        },
                    ) if kind == "haider.provider.idle_timeout.v1" => Some(data),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            evidence,
            vec![error.details.as_ref().expect("details")["idle_timeout"].clone()]
        );
    }
    (outcome, elapsed)
}

fn provider(script: Vec<FakeStep>) -> Arc<IdleProvider> {
    Arc::new(IdleProvider {
        fake: FakeProvider::new(script),
        stall_open: false,
        open_timeout: false,
    })
}

fn failure(delay_ms: u64) -> FakeStep {
    FakeStep::Error {
        kind: ProviderErrorKind::Transport,
        message: "connection reset by fake peer".into(),
        retry_after_ms: Some(delay_ms),
    }
}

#[tokio::test(start_paused = true)]
async fn retries_share_idle_budget_and_retain_transport_cause() {
    let provider = provider(vec![
        FakeStep::Delay { ms: 1_000 },
        failure(500),
        FakeStep::Delay { ms: 1_000 },
        failure(500),
        FakeStep::Hang,
    ]);
    let (outcome, elapsed) = run(provider.clone()).await;
    let error = outcome.error.expect("idle error");
    assert_eq!(error.code, ErrorCode::IdleTimeout);
    assert!(!error.retryable);
    assert_eq!(provider.fake.requests().len(), 3);
    assert!(
        elapsed <= BUDGET,
        "{elapsed:?} exceeds configured idle budget {BUDGET:?}"
    );
    let details = error.details.expect("typed details");
    assert_eq!(details["reason"], "idle_timeout");
    let idle = &details["idle_timeout"];
    assert_eq!(idle["attempts"], 3);
    assert_eq!(idle["elapsed_ms"], BUDGET.as_millis() as u64);
    assert_eq!(idle["idle_elapsed_ms"], idle["budget_ms"]);
    assert_eq!(idle["cause"]["kind"], "transport");
    assert_eq!(idle["cause"]["message"], "connection reset by fake peer");
    assert_eq!(idle["cause"]["retryable"], true);
    eprintln!(
        "idle retry measurement: elapsed_ms={} attempts=3 budget_ms={}",
        elapsed.as_millis(),
        BUDGET.as_millis()
    );
}

#[tokio::test(start_paused = true)]
async fn idle_exhaustion_interrupts_backoff_before_another_attempt() {
    let provider = provider(vec![failure(8_000), FakeStep::Hang]);
    let (outcome, elapsed) = run(provider.clone()).await;
    assert_eq!(outcome.error.expect("error").code, ErrorCode::IdleTimeout);
    assert_eq!(provider.fake.requests().len(), 1);
    assert!(elapsed <= BUDGET);
}

#[tokio::test(start_paused = true)]
async fn initial_response_open_stall_is_bounded_by_idle_budget() {
    let provider = Arc::new(IdleProvider {
        fake: FakeProvider::new(vec![]),
        stall_open: true,
        open_timeout: false,
    });
    let (outcome, elapsed) = run(provider).await;
    let error = outcome.error.expect("error");
    assert_eq!(error.code, ErrorCode::IdleTimeout);
    assert_eq!(
        error.details.expect("details")["idle_timeout"]["attempts"],
        1
    );
    assert!(elapsed <= BUDGET);
}

#[tokio::test(start_paused = true)]
async fn progressing_stream_can_outlive_total_idle_budget() {
    let mut script = Vec::new();
    for _ in 0..5 {
        script.push(FakeStep::Delay { ms: 3_000 });
        script.push(FakeStep::EmitText {
            text: "progress".into(),
        });
    }
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let (outcome, elapsed) = run(provider(script)).await;
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(elapsed, Duration::from_secs(15));
    eprintln!(
        "paced progress measurement: elapsed_ms={} budget_ms={} outcome=done",
        elapsed.as_millis(),
        BUDGET.as_millis()
    );
}

#[tokio::test(start_paused = true)]
async fn genuine_request_timeout_retains_provider_timeout() {
    let provider = Arc::new(IdleProvider {
        fake: FakeProvider::new(vec![]),
        stall_open: false,
        open_timeout: true,
    });
    let (outcome, elapsed) = run(provider).await;
    let error = outcome.error.expect("error");
    assert_eq!(error.code, ErrorCode::ProviderTimeout);
    assert_eq!(error.details.expect("details")["reason"], "response_open");
    assert!(elapsed < BUDGET);
}

#[tokio::test(start_paused = true)]
async fn stall_after_progress_expires_from_last_frame_without_retrying_content() {
    let provider = provider(vec![
        FakeStep::Delay { ms: 3_000 },
        FakeStep::EmitText {
            text: "first".into(),
        },
        FakeStep::Delay { ms: 3_000 },
        FakeStep::EmitText {
            text: "second".into(),
        },
        FakeStep::Hang,
    ]);
    let (outcome, elapsed) = run(provider.clone()).await;
    let error = outcome.error.expect("idle error after progress");
    assert_eq!(error.code, ErrorCode::IdleTimeout);
    assert_eq!(elapsed, Duration::from_secs(6) + BUDGET);
    assert_eq!(
        provider.fake.requests().len(),
        1,
        "committed content is never retried"
    );
    let details = error.details.expect("details");
    assert_eq!(
        details["idle_timeout"]["idle_elapsed_ms"],
        BUDGET.as_millis() as u64
    );
}

#[tokio::test(start_paused = true)]
async fn retryable_transport_failure_can_recover_before_idle_exhaustion() {
    let provider = provider(vec![
        failure(500),
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let (outcome, elapsed) = run(provider.clone()).await;
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.fake.requests().len(), 2);
    assert!(elapsed < BUDGET);
}
