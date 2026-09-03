#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::cache::CacheRequestAttemptV1;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::headless::{
    HeadlessRunEventPayload, HeadlessRunSpecV1, HeadlessRunUsageV1, RunBudgetDecisionReasonV1,
    RunBudgetDimensionV1, RunBudgetExhaustedV1, RunBudgetV1, RunDeadlineExceededV1,
};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::item::ItemEvent;
use haider_protocol::provider::{
    CacheBreakpointHashesV1, CacheControlObservationV1, CachePrefixMatchV1,
    CacheRequestDiagnosticV1, FinishReason, Usage, UsageSource,
};
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};
use haider_provider::{
    FakeProvider, FakeStep, Provider, ProviderError, ProviderStream, TurnRequest,
};
use haider_store::TurnCancelCommand;
use haider_store::{EventStore, Store};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, Instant, timeout};

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::turn_recovery::{
    STARTUP_HYDRATION_PAYLOAD_KINDS, interrupted_recovery_payloads_for_test,
};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, QueuedBudgetArm, QueuedBudgetWake, ResolvedTurnProvider,
    WorkerDependencies, WorkerManager, budget_request_is_unresolved_for_test,
    budget_usage_from_envelopes_for_test, exhausted_budget,
    projected_time_budget_exhaustion_for_test,
    route_retry_unresolved_attempt_is_chargeable_for_test, signal_queued_budget_change,
    wait_for_queued_budget_deadline_or_change,
};
use haider_core::{SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand};
use haider_protocol::session::SessionMetadataV1;

#[tokio::test(start_paused = true)]
async fn queued_budget_arm_survives_select_reentry_and_rearms_after_queue_change() {
    let queue_changed = Arc::new(tokio::sync::Notify::new());
    let scans = Arc::new(AtomicUsize::new(0));
    let run_id = RunId::new("queued-budget-arm");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(25);
    let make_arm = |queue_epoch| {
        let queue_changed = Arc::clone(&queue_changed);
        let scans = Arc::clone(&scans);
        QueuedBudgetArm::new(run_id.clone(), queue_epoch, async move {
            scans.fetch_add(1, Ordering::SeqCst);
            loop {
                let delay = deadline.saturating_duration_since(tokio::time::Instant::now());
                if wait_for_queued_budget_deadline_or_change(delay, &queue_changed).await
                    == QueuedBudgetWake::Deadline
                {
                    return;
                }
            }
        })
    };
    let mut queue_epoch = 0;
    let mut budget = make_arm(queue_epoch);

    for _ in 0..64 {
        tokio::select! {
            biased;
            () = budget.future_mut() => panic!("queued budget fired before its deadline"),
            () = std::future::ready(()) => {}
        }
    }
    assert_eq!(scans.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_millis(10)).await;
    signal_queued_budget_change(&mut queue_epoch, &queue_changed);
    assert!(!budget.matches(&run_id, queue_epoch));
    drop(budget);
    let mut budget = make_arm(queue_epoch);

    tokio::select! {
        biased;
        () = budget.future_mut() => panic!("queue mutation fired the budget early"),
        () = std::future::ready(()) => {}
    }
    assert_eq!(scans.load(Ordering::SeqCst), 2);

    for _ in 0..64 {
        tokio::select! {
            biased;
            () = budget.future_mut() => panic!("rearmed queued budget fired early"),
            () = std::future::ready(()) => {}
        }
    }
    tokio::time::advance(Duration::from_millis(15)).await;
    budget.future_mut().await;
    assert_eq!(scans.load(Ordering::SeqCst), 2);
}

/// MUTATION CHECK: charge the unmatched route attempt as missing usage and a
/// recovered retry terminalizes before transport; suppress all future missing
/// attempts and a genuinely lost usage projection escapes the hard budget.
#[test]
fn route_retry_supersedes_only_the_admitted_unreported_physical_attempts() {
    assert!(!route_retry_unresolved_attempt_is_chargeable_for_test(1, 1));
    assert!(!route_retry_unresolved_attempt_is_chargeable_for_test(2, 2));
    assert!(route_retry_unresolved_attempt_is_chargeable_for_test(2, 1));
    assert!(route_retry_unresolved_attempt_is_chargeable_for_test(3, 2));
}

fn envelope(seq: u64, run_id: &RunId, payload: serde_json::Value) -> RawEnvelope {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "event_id": format!("budget-event-{seq}"),
        "seq": seq,
        "session_id": "budget-session",
        "run_id": run_id,
        "device_id": "budget-device",
        "authority_epoch": 1,
        "worker_generation": 1,
        "committed_at_ms": seq,
        "render": {"ui": false, "durable": true, "prompt": "omit"},
        "payload": payload,
    }))
    .expect("budget envelope")
}

fn headless_spec() -> HeadlessRunSpecV1 {
    HeadlessRunSpecV1 {
        cwd: "budget-workspace".to_owned(),
        provider: "fake".to_owned(),
        model: "budget-model".to_owned(),
        max_output_tokens: 64,
        effort: None,
        fast: false,
        seed: Some(7),
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        budget: RunBudgetV1 {
            max_time_ms: Some(25),
            ..RunBudgetV1::default()
        },
        request_deadline_unix_ms: None,
        replay_of: None,
    }
}

#[test]
fn budget_exhaustion_is_a_typed_fact_and_terminal_error_code() {
    let usage = HeadlessRunUsageV1 {
        logical_input_tokens: 90,
        billed_output_tokens: 10,
        cache_read_tokens: 40,
        cache_write_tokens: 5,
        total_tokens: 100,
        estimated_cost_microusd: Some(321),
        elapsed_ms: 25,
        ..HeadlessRunUsageV1::default()
    };
    let payload = HeadlessRunEventPayload::RunBudgetExhausted(RunBudgetExhaustedV1 {
        dimension: RunBudgetDimensionV1::Tokens,
        limit: 100,
        usage,
        decision: None,
    });
    let encoded = payload.to_payload_value().expect("budget payload encodes");
    assert_eq!(encoded["type"], "run_budget_exhausted");
    assert_eq!(encoded["usage"]["cache_read_tokens"], 40);
    assert_eq!(encoded["usage"]["cache_write_tokens"], 5);
    assert_eq!(ErrorCode::BudgetExhausted.as_str(), "budget_exhausted");
    assert_eq!(
        HeadlessRunEventPayload::from_payload_value(&encoded),
        Some(payload)
    );
}

#[test]
fn request_deadline_is_a_distinct_typed_durable_fact() {
    let payload = HeadlessRunEventPayload::RunDeadlineExceeded(RunDeadlineExceededV1 {
        deadline_unix_ms: 4_200,
    });
    let encoded = payload
        .to_payload_value()
        .expect("deadline payload encodes");
    assert_eq!(encoded["type"], "run_deadline_exceeded");
    assert_eq!(encoded["deadline_unix_ms"], 4_200);
    assert_eq!(
        HeadlessRunEventPayload::from_payload_value(&encoded),
        Some(payload)
    );
}

#[test]
fn token_and_cost_limits_use_the_canonical_usage_without_losing_cache_counters() {
    let usage = HeadlessRunUsageV1 {
        logical_input_tokens: 80,
        billed_output_tokens: 15,
        additional_reasoning_tokens: 5,
        cache_read_tokens: 30,
        cache_write_tokens: 7,
        total_tokens: 100,
        estimated_cost_microusd: Some(250),
        elapsed_ms: 9,
    };
    let token = exhausted_budget(
        &RunBudgetV1 {
            max_tokens: Some(100),
            ..RunBudgetV1::default()
        },
        usage.clone(),
        "openai",
        "gpt-5.6-sol",
    )
    .expect("token ceiling");
    assert_eq!(token.dimension, RunBudgetDimensionV1::Tokens);
    assert_eq!(token.usage.cache_read_tokens, 30);
    assert_eq!(token.usage.cache_write_tokens, 7);

    let cost = exhausted_budget(
        &RunBudgetV1 {
            max_cost_microusd: Some(250),
            ..RunBudgetV1::default()
        },
        usage,
        "openai",
        "gpt-5.6-sol",
    )
    .expect("cost ceiling");
    assert_eq!(cost.dimension, RunBudgetDimensionV1::Cost);
}

#[test]
fn an_unpriced_nonzero_run_fails_a_configured_cost_budget_closed() {
    let usage = HeadlessRunUsageV1 {
        logical_input_tokens: 1,
        total_tokens: 1,
        estimated_cost_microusd: None,
        ..HeadlessRunUsageV1::default()
    };
    let exhausted = exhausted_budget(
        &RunBudgetV1 {
            max_cost_microusd: Some(1),
            ..RunBudgetV1::default()
        },
        usage,
        "custom-provider",
        "unpriced-model",
    )
    .expect("unknown pricing cannot bypass a cost limit");
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Cost);
    assert!(matches!(
        exhausted.decision.map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::PricingUnavailable { provider, model })
            if provider == "custom-provider" && model == "unpriced-model"
    ));
}

/// MUTATION CHECK: remove the time arm from the physical-request projection
/// reducer and this direct seam test returns no exhaustion.
#[test]
fn projected_request_seam_rejects_elapsed_time_without_a_candidate_projection() {
    let exhausted = projected_time_budget_exhaustion_for_test(
        &RunBudgetV1 {
            max_time_ms: Some(25),
            ..RunBudgetV1::default()
        },
        HeadlessRunUsageV1 {
            elapsed_ms: 25,
            ..HeadlessRunUsageV1::default()
        },
    )
    .expect("time cap binds at provider admission");
    let decision = exhausted.decision.expect("time decision");
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Time);
    assert_eq!(decision.spent, 25);
    assert_eq!(decision.cap, 25);
    assert_eq!(decision.projected, None);
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::TimeElapsed);
}

#[test]
fn unknown_future_budget_dimensions_decode_without_losing_the_terminal_fact() {
    let encoded = serde_json::json!({
        "type": "run_budget_exhausted",
        "dimension": "requests",
        "limit": 3,
        "usage": {
            "logical_input_tokens": 0,
            "billed_output_tokens": 0,
            "additional_reasoning_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "total_tokens": 0,
            "elapsed_ms": 1
        }
    });
    let decoded = HeadlessRunEventPayload::from_payload_value(&encoded);
    assert!(matches!(
        decoded,
        Some(HeadlessRunEventPayload::RunBudgetExhausted(
            RunBudgetExhaustedV1 {
                dimension: RunBudgetDimensionV1::Unknown,
                ..
            }
        ))
    ));
}

#[test]
fn journal_usage_fold_replaces_snapshots_and_keeps_cache_lifecycle_requests() {
    let run_id = RunId::new("budget-run");
    let usage = |request_kind: &str,
                 ordinal: u64,
                 logical: u64,
                 output: u64,
                 reasoning: u64,
                 read: u64,
                 write: u64| {
        serde_json::json!({
            "type": "usage",
            "input": logical,
            "output": output,
            "reasoning": reasoning,
            "cached": read,
            "source": "locally_exact",
            "scope": {
                "provider": "openai",
                "model": "gpt-5",
                "auth_scope": "budget-test",
                "cache_epoch": "epoch-1",
                "request_kind": request_kind,
                "run": run_id,
            },
            "request": {
                "ordinal": ordinal,
                "input": logical,
                "output": output,
                "reasoning": reasoning,
                "cached": read,
                "source": "locally_exact",
                "normalized": {
                    "logical_input": logical,
                    "uncached_input": logical - read,
                    "cache_read_input": read,
                    "cache_write_input": write,
                    "billed_output": output,
                    "reasoning_detail": reasoning,
                    "reasoning_accounting": "additional_to_output",
                    "cache_telemetry_input": logical,
                },
            },
        })
    };
    let envelopes = vec![
        envelope(1, &run_id, usage("main_turn", 0, 100, 10, 2, 40, 4)),
        // Same physical request: this later cumulative snapshot replaces it.
        envelope(2, &run_id, usage("main_turn", 0, 120, 12, 4, 50, 5)),
        // Same ordinal but a distinct cache/compaction request lane: add it.
        envelope(3, &run_id, usage("compaction", 0, 30, 3, 1, 10, 2)),
    ];
    let folded = budget_usage_from_envelopes_for_test(
        &SessionId::new("budget-session"),
        &run_id,
        "gpt-5",
        envelopes,
    );
    assert_eq!(folded.logical_input_tokens, 150);
    assert_eq!(folded.billed_output_tokens, 15);
    assert_eq!(folded.additional_reasoning_tokens, 5);
    assert_eq!(folded.cache_read_tokens, 60);
    assert_eq!(folded.cache_write_tokens, 7);
    assert_eq!(folded.total_tokens, 170);
    assert!(folded.estimated_cost_microusd.is_some_and(|cost| cost > 0));
}

/// MUTATION CHECK: ignore durable cache-request attempts during coordinator
/// reconstruction and this crash-boundary request is treated as free.
#[test]
fn restart_detects_a_durable_provider_attempt_without_reconciled_usage() {
    let run_id = RunId::new("budget-restart-unresolved-run");
    let item = CacheRequestAttemptV1 {
        ordinal: 1,
        diagnostic: CacheRequestDiagnosticV1 {
            history_message_count: 1,
            stable_prefix_tokens: 8,
            breakpoint_hashes: CacheBreakpointHashesV1::default(),
            cache_domain_hash: Some("budget-domain".into()),
            cache_domain_changed: None,
            previous_breakpoint: None,
            prefix_match: CachePrefixMatchV1::Unavailable,
            control: CacheControlObservationV1::NotRequired,
            cacheable_minimum_tokens: None,
            reuse_gap_ms: None,
            rewarm: None,
            classification: None,
        },
    }
    .extension_item()
    .expect("cache-attempt extension");
    let attempt = envelope(
        1,
        &run_id,
        serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("budget-restart-attempt"),
            item,
        }))
        .expect("attempt payload"),
    );
    assert!(budget_request_is_unresolved_for_test(
        &SessionId::new("budget-session"),
        &run_id,
        "gpt-5.6-sol",
        vec![attempt],
    ));
}

/// MUTATION CHECK: swap the terminal tail or change `Errored` to
/// `Cancelled`. Expected runtime failure: the durable budget cause no longer
/// precedes the session-idle fact in the one transactional recovery batch.
#[test]
fn restart_recovery_keeps_a_durable_budget_cause_ahead_of_cancellation() {
    assert!(
        STARTUP_HYDRATION_PAYLOAD_KINDS.contains(&"headless_run_configured")
            && STARTUP_HYDRATION_PAYLOAD_KINDS.contains(&"run_budget_exhausted")
    );
    let run_id = RunId::new("budget-recovery-run");
    let exhausted = RunBudgetExhaustedV1 {
        dimension: RunBudgetDimensionV1::Time,
        limit: 25,
        usage: HeadlessRunUsageV1 {
            elapsed_ms: 25,
            ..HeadlessRunUsageV1::default()
        },
        decision: None,
    };
    let envelopes = vec![
        envelope(
            1,
            &run_id,
            HeadlessRunEventPayload::HeadlessRunConfigured(headless_spec())
                .to_payload_value()
                .expect("headless config payload"),
        ),
        envelope(
            2,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Streaming))
                .expect("streaming payload"),
        ),
        envelope(
            3,
            &run_id,
            HeadlessRunEventPayload::RunBudgetExhausted(exhausted)
                .to_payload_value()
                .expect("budget payload"),
        ),
    ];
    let payloads = interrupted_recovery_payloads_for_test(&run_id, &envelopes, true);
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::BudgetExhausted,
            ..
        }
    )));
    assert!(payloads.ends_with(&[
        EventPayload::RunState(RunState::Errored),
        EventPayload::SessionState(SessionState::Idle { interrupted: true }),
    ]));
    assert!(!payloads.contains(&EventPayload::RunState(RunState::Cancelled)));
}

/// MUTATION CHECK: change the terminal tail to `Errored` or move it after
/// session idle. Expected runtime failure: an unconfigured budget fact is
/// promoted or the single-batch run/session ordering is no longer proved.
#[test]
fn restart_recovery_does_not_promote_a_budget_fact_without_headless_configuration() {
    let run_id = RunId::new("ordinary-budget-recovery-run");
    let envelopes = vec![
        envelope(
            1,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Streaming))
                .expect("streaming payload"),
        ),
        envelope(
            2,
            &run_id,
            HeadlessRunEventPayload::RunBudgetExhausted(RunBudgetExhaustedV1 {
                dimension: RunBudgetDimensionV1::Time,
                limit: 25,
                usage: HeadlessRunUsageV1 {
                    elapsed_ms: 25,
                    ..HeadlessRunUsageV1::default()
                },
                decision: None,
            })
            .to_payload_value()
            .expect("unconfigured budget payload"),
        ),
    ];
    let payloads = interrupted_recovery_payloads_for_test(&run_id, &envelopes, true);
    assert!(payloads.ends_with(&[
        EventPayload::RunState(RunState::Cancelled),
        EventPayload::SessionState(SessionState::Idle { interrupted: true }),
    ]));
    assert!(!payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::BudgetExhausted,
            ..
        }
    )));
}

#[test]
fn a_durable_budget_cause_wins_when_stop_races_the_terminal_batch() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let run_id = RunId::new("budget-stop-race-run");
    let exhausted = RunBudgetExhaustedV1 {
        dimension: RunBudgetDimensionV1::Time,
        limit: 25,
        usage: HeadlessRunUsageV1 {
            elapsed_ms: 25,
            ..HeadlessRunUsageV1::default()
        },
        decision: None,
    };

    let mut configured_and_streaming = [
        envelope(
            1,
            &run_id,
            HeadlessRunEventPayload::HeadlessRunConfigured(headless_spec())
                .to_payload_value()
                .expect("headless config payload"),
        ),
        envelope(
            2,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Streaming))
                .expect("streaming payload"),
        ),
    ];
    store
        .append(&mut configured_and_streaming)
        .expect("seed configured streaming run");

    let mut budget_fact = [envelope(
        3,
        &run_id,
        HeadlessRunEventPayload::RunBudgetExhausted(exhausted)
            .to_payload_value()
            .expect("budget payload"),
    )];
    store
        .append_worker(&mut budget_fact)
        .expect("journal durable budget cause");

    // This is the exact race order: stop commits after the budget cause but
    // before the worker's atomic typed failure + terminal-state batch.
    let mut cancelling = [envelope(
        4,
        &run_id,
        serde_json::to_value(EventPayload::RunState(RunState::Cancelling))
            .expect("cancelling payload"),
    )];
    store.append(&mut cancelling).expect("race stop request");

    let mut budget_terminal = [
        envelope(
            5,
            &run_id,
            serde_json::to_value(EventPayload::RunFailed {
                code: ErrorCode::BudgetExhausted,
                message: "headless time budget exhausted".to_owned(),
                retryable: false,
                presentation: None,
            })
            .expect("budget failure payload"),
        ),
        envelope(
            6,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Errored))
                .expect("errored payload"),
        ),
    ];
    store
        .append_worker(&mut budget_terminal)
        .expect("budget terminal wins raced stop");

    let journal = store
        .read(&SessionId::new("budget-session"), 0, 16)
        .expect("read raced journal");
    assert!(journal.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::RunFailed {
            code: ErrorCode::BudgetExhausted,
            ..
        })
    )));
    assert!(matches!(
        journal
            .last()
            .and_then(|event| event.payload.decode_event().ok()),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

#[test]
fn a_budget_fact_without_headless_configuration_is_rejected() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let run_id = RunId::new("budget-no-config-run");
    let mut streaming = [envelope(
        1,
        &run_id,
        serde_json::to_value(EventPayload::RunState(RunState::Streaming))
            .expect("streaming payload"),
    )];
    store.append(&mut streaming).expect("seed ordinary run");

    let mut budget_fact = [envelope(
        2,
        &run_id,
        HeadlessRunEventPayload::RunBudgetExhausted(RunBudgetExhaustedV1 {
            dimension: RunBudgetDimensionV1::Time,
            limit: 25,
            usage: HeadlessRunUsageV1 {
                elapsed_ms: 25,
                ..HeadlessRunUsageV1::default()
            },
            decision: None,
        })
        .to_payload_value()
        .expect("budget payload"),
    )];
    let error = store
        .append_worker(&mut budget_fact)
        .expect_err("ordinary run must reject a budget cause");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn a_budget_fact_without_headless_configuration_cannot_override_cancellation() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let run_id = RunId::new("budget-stop-no-cause-run");
    let exhausted = RunBudgetExhaustedV1 {
        dimension: RunBudgetDimensionV1::Time,
        limit: 25,
        usage: HeadlessRunUsageV1 {
            elapsed_ms: 25,
            ..HeadlessRunUsageV1::default()
        },
        decision: None,
    };
    let mut states_and_untrusted_fact = [
        envelope(
            1,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Streaming))
                .expect("streaming payload"),
        ),
        envelope(
            2,
            &run_id,
            HeadlessRunEventPayload::RunBudgetExhausted(exhausted)
                .to_payload_value()
                .expect("untrusted budget payload"),
        ),
        envelope(
            3,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Cancelling))
                .expect("cancelling payload"),
        ),
    ];
    // Ordinary append deliberately simulates a malformed/legacy producer
    // bypassing worker admission; transition authorization must still fail.
    store
        .append(&mut states_and_untrusted_fact)
        .expect("seed unconfigured budget fact and cancellation");
    let mut budget_terminal = [
        envelope(
            4,
            &run_id,
            serde_json::to_value(EventPayload::RunFailed {
                code: ErrorCode::BudgetExhausted,
                message: "unproven budget failure".to_owned(),
                retryable: false,
                presentation: None,
            })
            .expect("budget failure payload"),
        ),
        envelope(
            5,
            &run_id,
            serde_json::to_value(EventPayload::RunState(RunState::Errored))
                .expect("errored payload"),
        ),
    ];
    let error = store
        .append_worker(&mut budget_terminal)
        .expect_err("unconfigured budget terminal must not override cancellation");
    assert_eq!(error.code, ErrorCode::RunNotActive);
}

struct NeverOpensProvider {
    fallback: FakeProvider,
    requests: AtomicUsize,
    in_flight: Arc<AtomicUsize>,
}

impl NeverOpensProvider {
    fn new(in_flight: Arc<AtomicUsize>) -> Self {
        Self {
            fallback: FakeProvider::new(Vec::new()),
            requests: AtomicUsize::new(0),
            in_flight,
        }
    }
}

struct InFlightRequest(Arc<AtomicUsize>);

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Provider for NeverOpensProvider {
    fn trusts_default_route_absence(&self) -> bool {
        true
    }

    fn route_status(&self) -> haider_platform::RouteStatus {
        haider_platform::RouteStatus::Available
    }

    async fn capabilities(&self) -> haider_protocol::provider::CapabilityDoc {
        self.fallback.capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _guard = InFlightRequest(Arc::clone(&self.in_flight));
        std::future::pending().await
    }
}

struct DeadlineProviderFactory {
    provider: Arc<NeverOpensProvider>,
}

struct FakeBudgetProviderFactory {
    provider: Arc<FakeProvider>,
}

enum BudgetCaseAction<'a> {
    Subturn(&'a str),
    Cancel,
}

struct BudgetCaseOptions<'a> {
    action: Option<BudgetCaseAction<'a>>,
    native_pdf: Option<Vec<u8>>,
}

// Registry #94: action cases first spend at most 1 s waiting for request one.
// After that separate bound returns, terminal reconciliation gets 4 s plus
// one 10 ms journal-poll interval: the complete action case is bounded by
// 1,000 ms + 4,010 ms = 5,010 ms.
const BUDGET_CASE_DEADLINE: Duration = Duration::from_millis(4_010);
const FAKE_REQUEST_START_DEADLINE: Duration = Duration::from_secs(1);

#[async_trait::async_trait]
impl ProviderFactory for FakeBudgetProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

async fn run_provider_budget_case(
    label: &str,
    provider_name: &str,
    model: &str,
    max_output_tokens: u64,
    budget: RunBudgetV1,
    script: Vec<FakeStep>,
) -> (usize, Vec<RawEnvelope>) {
    run_provider_budget_case_with_action(
        label,
        provider_name,
        model,
        max_output_tokens,
        budget,
        script,
        None,
    )
    .await
}

async fn run_provider_budget_case_with_action(
    label: &str,
    provider_name: &str,
    model: &str,
    max_output_tokens: u64,
    budget: RunBudgetV1,
    script: Vec<FakeStep>,
    action: Option<BudgetCaseAction<'_>>,
) -> (usize, Vec<RawEnvelope>) {
    run_provider_budget_case_inner(
        label,
        provider_name,
        model,
        max_output_tokens,
        budget,
        script,
        BudgetCaseOptions {
            action,
            native_pdf: None,
        },
    )
    .await
}

async fn run_provider_budget_case_with_native_pdf(
    label: &str,
    max_output_tokens: u64,
    budget: RunBudgetV1,
    script: Vec<FakeStep>,
    pdf_bytes: Vec<u8>,
) -> (usize, Vec<RawEnvelope>) {
    run_provider_budget_case_inner(
        label,
        "openai",
        "gpt-5.6-sol",
        max_output_tokens,
        budget,
        script,
        BudgetCaseOptions {
            action: None,
            native_pdf: Some(pdf_bytes),
        },
    )
    .await
}

async fn run_provider_budget_case_inner(
    label: &str,
    provider_name: &str,
    model: &str,
    max_output_tokens: u64,
    budget: RunBudgetV1,
    script: Vec<FakeStep>,
    options: BudgetCaseOptions<'_>,
) -> (usize, Vec<RawEnvelope>) {
    let BudgetCaseOptions { action, native_pdf } = options;
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let provider = Arc::new(if native_pdf.is_some() {
        FakeProvider::new(script).with_pdf_documents_native()
    } else {
        FakeProvider::new(script)
    });
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FakeBudgetProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(crate::delegation::DelegationHandle::new(hub.clone())),
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install worker manager");

    let session_id = SessionId::new(format!("{label}-session"));
    let run_id = RunId::new(format!("{label}-run"));
    let device_id = DeviceId::new(format!("{label}-device"));
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("{label}-create"),
        request_digest: format!("{label}-create-digest"),
        request_json: format!(r#"{{"session":"{label}"}}"#),
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        provider: provider_name.to_owned(),
        model: model.to_owned(),
        max_tokens: max_output_tokens,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("{label}-created")),
        device_id: device_id.clone(),
    })
    .await
    .expect("create session");
    let spec = HeadlessRunSpecV1 {
        cwd,
        provider: provider_name.to_owned(),
        model: model.to_owned(),
        max_output_tokens,
        effort: None,
        fast: false,
        seed: None,
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        budget,
        request_deadline_unix_ms: None,
        replay_of: None,
    };
    let attachments = if let Some(bytes) = native_pdf {
        let artifact = store.put(bytes).await.expect("store native PDF");
        vec![AttachmentBlock::Pdf {
            artifact,
            name: "budget.pdf".into(),
            pages: 1,
            delivery: PdfDeliveryMode::NativeDocument,
        }]
    } else {
        Vec::new()
    };
    let request_json = serde_json::json!({
        "session_id": &session_id,
        "text": "exercise the provider budget",
        "headless": spec,
    })
    .to_string();
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: format!("{label}-turn"),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "exercise the provider budget".into(),
            attachments,
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("{label}-queued")),
            user_event_id: EventId::new(format!("{label}-user")),
            active_event_id: EventId::new(format!("{label}-active")),
            device_id,
        })
        .await
        .expect("accept headless turn");
    let accepted_seq = accepted.accepted_seq;
    manager
        .handle()
        .submit(accepted)
        .await
        .expect("submit headless turn");
    if let Some(action) = action {
        timeout(FAKE_REQUEST_START_DEADLINE, async {
            while provider.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first fake-provider request starts");
        match action {
            BudgetCaseAction::Subturn(text) => manager
                .handle()
                .subturn(
                    session_id.clone(),
                    run_id.clone(),
                    accepted_seq.saturating_add(1),
                    text.to_owned(),
                )
                .await
                .expect("deliver budget subturn"),
            BudgetCaseAction::Cancel => {
                let request_json = serde_json::json!({
                    "session_id": &session_id,
                    "run_id": &run_id,
                    "reason": "budget-test",
                })
                .to_string();
                hub.cancel_internal_turn(TurnCancelCommand {
                    command_id: format!("{label}-cancel"),
                    request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation: hub.worker_generation(),
                    run_id: run_id.clone(),
                    cancelling_event_id: EventId::new(format!("{label}-cancelling")),
                    device_id: DeviceId::new(format!("{label}-cancel-device")),
                })
                .await
                .expect("cancel budget request");
            }
        }
    }
    let events = timeout(BUDGET_CASE_DEADLINE, async {
        loop {
            let events = store.read(&session_id, 0, 1024).await.expect("read run");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&run_id)
                    && event.payload.decode_event()
                        .is_ok_and(|payload| {
                            matches!(payload, EventPayload::RunState(state) if state.is_terminal())
                        })
            }) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("budget case terminalizes");
    let request_count = provider.requests().len();
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
    (request_count, events)
}

fn budget_fact(events: &[RawEnvelope]) -> RunBudgetExhaustedV1 {
    events
        .iter()
        .find_map(|event| {
            HeadlessRunEventPayload::from_payload_value(&event.payload).and_then(|payload| {
                match payload {
                    HeadlessRunEventPayload::RunBudgetExhausted(exhausted) => Some(exhausted),
                    HeadlessRunEventPayload::HeadlessRunConfigured(_)
                    | HeadlessRunEventPayload::RunDeadlineExceeded(_) => None,
                }
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "typed budget fact; payloads={:?}",
                events
                    .iter()
                    .map(|event| &event.payload)
                    .collect::<Vec<_>>()
            )
        })
}

/// MUTATION CHECK: move provider-budget admission after `stream_turn` and
/// this sends one request instead of zero.
#[tokio::test]
async fn projected_first_request_over_cap_sends_zero_provider_requests() {
    let (requests, events) = run_provider_budget_case(
        "budget-preflight-zero",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(1),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitText {
                text: "must stay unused".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    assert_eq!(requests, 0);
    let exhausted = budget_fact(&events);
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Cost);
    assert!(matches!(
        exhausted.decision.as_ref().map(|decision| &decision.reason),
        Some(RunBudgetDecisionReasonV1::ProjectedRequest)
    ));
    assert_eq!(
        exhausted.decision.as_ref().map(|decision| decision.spent),
        Some(0)
    );
    assert!(
        exhausted
            .decision
            .as_ref()
            .and_then(|decision| decision.projected)
            .is_some_and(|projected| projected > 1)
    );
}

/// MUTATION CHECK: omit resolved native-document bytes from the request
/// estimate and this 512 KiB PDF is sent under the metadata-only projection.
#[tokio::test]
async fn native_pdf_bytes_bind_the_token_cap_before_the_first_request() {
    let base64_len = 512_u64 * 1024 * 4 / 3;
    let (requests, events) = run_provider_budget_case_with_native_pdf(
        "budget-native-pdf-preflight",
        64,
        RunBudgetV1 {
            max_tokens: Some(100_000),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
        vec![b'P'; 512 * 1024],
    )
    .await;
    assert_eq!(requests, 0);
    let exhausted = budget_fact(&events);
    let decision = exhausted.decision.expect("native PDF projection");
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Tokens);
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert_eq!(decision.spent, 0);
    assert!(
        decision
            .projected
            .is_some_and(|projected| projected >= base64_len / 4),
        "projection includes the resolved document payload"
    );
}

#[tokio::test]
async fn projected_token_budget_is_checked_at_the_same_preflight_seam() {
    let (requests, events) = run_provider_budget_case(
        "budget-token-preflight",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_tokens: Some(1),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
    )
    .await;
    assert_eq!(requests, 0);
    let exhausted = budget_fact(&events);
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Tokens);
    assert!(matches!(
        exhausted.decision.map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::ProjectedRequest)
    ));
}

#[tokio::test]
async fn projected_second_request_over_cap_sends_exactly_one_provider_request() {
    let usage = Usage {
        input: 180_000,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let (requests, events) = run_provider_budget_case(
        "budget-preflight-second",
        "openai",
        "gpt-5.6-sol",
        4_096,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitUsage { usage },
            FakeStep::EmitToolCall {
                call_id: "budget-todo".into(),
                name: "todo_write".into(),
                args: serde_json::json!({
                    "items": [{"text": "continue", "status": "in_progress"}]
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "budget-todo".into(),
            },
            FakeStep::EmitText {
                text: "second request must stay unused".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    assert_eq!(requests, 1);
    let exhausted = budget_fact(&events);
    let decision = exhausted.decision.expect("projected decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert!(decision.spent < decision.cap);
    assert!(
        decision
            .projected
            .is_some_and(|projected| { decision.spent.saturating_add(projected) > decision.cap })
    );
}

/// MUTATION CHECK: move either compaction admission below its matching
/// provider-open call and the same capped run can pay for that request first.
#[test]
fn compaction_provider_paths_admit_budget_before_transport() {
    let source = include_str!("worker.rs");
    let impl_start = source
        .find("impl ContextCompactor for DaemonContextCompactor")
        .expect("context compactor implementation");
    let impl_tail = &source[impl_start..];
    let mut depth = 0_usize;
    let mut body_started = false;
    let impl_end = impl_tail
        .char_indices()
        .find_map(|(index, character)| match character {
            '{' => {
                body_started = true;
                depth = depth.saturating_add(1);
                None
            }
            '}' if body_started => {
                depth = depth.saturating_sub(1);
                (depth == 0).then_some(index + character.len_utf8())
            }
            _ => None,
        })
        .expect("complete context compactor implementation body");
    let impl_source = &impl_tail[..impl_end];
    let admissions = impl_source
        .match_indices(".admit_budget_request(")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let provider_opens = impl_source
        .match_indices("self.provider.stream_")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        admissions.len(),
        2,
        "replay and degraded fallback admissions"
    );
    assert_eq!(
        provider_opens.len(),
        2,
        "replay and degraded fallback sends"
    );
    assert!(
        admissions
            .iter()
            .zip(provider_opens.iter())
            .all(|(admission, open)| admission < open),
        "every compaction provider path must bind the budget before transport"
    );
}

#[tokio::test]
async fn streamed_usage_crossing_the_cap_stops_at_that_chunk_boundary() {
    let usage = Usage {
        input: 30_000,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let (requests, events) = run_provider_budget_case(
        "budget-stream-crossing",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(100_000),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitUsage { usage },
            FakeStep::EmitText {
                text: "must not cross the budget chunk".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    assert_eq!(requests, 1);
    let exhausted = budget_fact(&events);
    assert!(matches!(
        exhausted.decision.map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::ActualUsage)
    ));
    assert!(events.iter().all(|event| {
        !event
            .payload
            .to_string()
            .contains("must not cross the budget chunk")
    }));
}

#[tokio::test]
async fn unknown_provider_pricing_fails_closed_before_the_request() {
    let (requests, events) = run_provider_budget_case(
        "budget-pricing-unknown",
        "fake",
        "fake-model",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
    )
    .await;
    assert_eq!(requests, 0);
    let exhausted = budget_fact(&events);
    assert!(matches!(
        exhausted.decision.map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::PricingUnavailable { provider, model })
            if provider == "fake" && model == "fake-model"
    ));
}

#[tokio::test]
async fn unknown_pricing_is_named_even_when_a_token_projection_also_exceeds_its_cap() {
    let (requests, events) = run_provider_budget_case(
        "budget-pricing-unknown-combined",
        "fake",
        "fake-model",
        64,
        RunBudgetV1 {
            max_tokens: Some(1),
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
    )
    .await;
    assert_eq!(requests, 0);
    assert!(matches!(
        budget_fact(&events)
            .decision
            .map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::PricingUnavailable { provider, model })
            if provider == "fake" && model == "fake-model"
    ));
}

#[tokio::test]
async fn elapsed_time_is_checked_before_request_with_no_candidate_projection() {
    let (requests, events) = run_provider_budget_case(
        "budget-time-preflight",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_time_ms: Some(1),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
    )
    .await;
    assert_eq!(requests, 0);
    let decision = budget_fact(&events).decision.expect("time decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::TimeElapsed);
    assert_eq!(decision.projected, None);
}

#[tokio::test]
async fn missing_actual_usage_fails_closed_after_the_request() {
    let (requests, events) = run_provider_budget_case(
        "budget-usage-missing",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }],
    )
    .await;
    assert_eq!(requests, 1);
    let exhausted = budget_fact(&events);
    let decision = exhausted.decision.expect("missing-usage decision");
    assert!(matches!(
        decision.reason,
        RunBudgetDecisionReasonV1::UsageUnavailable { provider, model }
            if provider == "openai" && model == "gpt-5.6-sol"
    ));
    assert_eq!(decision.spent, 0);
    assert!(decision.projected.is_some());
}

/// MUTATION CHECK: remove the abandoned-request check from the next budget
/// admission and the held Subturn opens request two before final usage exists.
#[tokio::test]
async fn subturn_before_final_usage_cannot_open_a_second_provider_request() {
    let (requests, events) = run_provider_budget_case_with_action(
        "budget-subturn-unreported",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitText {
                text: "first request is live".into(),
            },
            FakeStep::Delay { ms: 200 },
            FakeStep::EmitToolCall {
                call_id: "budget-held-tool".into(),
                name: "todo_write".into(),
                args: serde_json::json!({
                    "items": [{"text": "held", "status": "in_progress"}]
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "second request must stay unused".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        Some(BudgetCaseAction::Subturn(
            "change course before the tool runs",
        )),
    )
    .await;
    assert_eq!(requests, 1);
    assert!(matches!(
        budget_fact(&events)
            .decision
            .map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::UsageUnavailable { .. })
    ));
}

/// MUTATION CHECK: skip `after_request` on the pending-Subturn continuation
/// and the durable usage below is misclassified as unavailable instead of
/// allowing the real spent+projection decision for request two.
#[tokio::test]
async fn subturn_after_reported_usage_reconciles_before_the_next_projection() {
    let usage = Usage {
        input: 180_000,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let (requests, events) = run_provider_budget_case_with_action(
        "budget-subturn-reported",
        "openai",
        "gpt-5.6-sol",
        4_096,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitUsage { usage },
            FakeStep::Delay { ms: 200 },
            FakeStep::EmitToolCall {
                call_id: "budget-reported-held-tool".into(),
                name: "todo_write".into(),
                args: serde_json::json!({
                    "items": [{"text": "held", "status": "in_progress"}]
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "request two must be rejected by its real projection".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        Some(BudgetCaseAction::Subturn(
            "change course after usage was reported",
        )),
    )
    .await;
    assert_eq!(requests, 1);
    let decision = budget_fact(&events)
        .decision
        .expect("second-request projection");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert!(decision.spent > 0);
    assert!(
        decision
            .projected
            .is_some_and(|projected| decision.spent.saturating_add(projected) > decision.cap)
    );
}

#[tokio::test]
async fn cancellation_after_send_reconciles_missing_usage_as_a_budget_stop() {
    let (requests, events) = run_provider_budget_case_with_action(
        "budget-cancel-unreported",
        "openai",
        "gpt-5.6-sol",
        64,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![FakeStep::Hang],
        Some(BudgetCaseAction::Cancel),
    )
    .await;
    assert_eq!(requests, 1);
    assert!(matches!(
        budget_fact(&events)
            .decision
            .map(|decision| decision.reason),
        Some(RunBudgetDecisionReasonV1::UsageUnavailable { .. })
    ));
}

#[tokio::test]
async fn child_usage_is_charged_to_the_parent_before_its_next_request() {
    let parent_usage = Usage {
        input: 100,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let child_usage = Usage {
        input: 180_000,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let (requests, events) = run_provider_budget_case(
        "budget-child-shared",
        "openai",
        "gpt-5.6-sol",
        4_096,
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        vec![
            FakeStep::EmitUsage {
                usage: parent_usage,
            },
            FakeStep::EmitToolCall {
                call_id: "budget-spawn".into(),
                name: "spawn_subagent".into(),
                args: serde_json::json!({
                    "task": "budget child",
                    "prompt": "report one result",
                    "budget_tokens": 4096
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitUsage { usage: child_usage },
            FakeStep::EmitText {
                text: "child report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::ExpectToolResult {
                call_id: "budget-spawn".into(),
            },
            FakeStep::EmitText {
                text: "parent request must stay unused".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    assert_eq!(
        requests,
        2,
        "only the parent admission and child exchange may reach the provider; payloads={:?}",
        events
            .iter()
            .map(|event| &event.payload)
            .collect::<Vec<_>>()
    );
    let exhausted = budget_fact(&events);
    let decision = exhausted.decision.expect("parent projected decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert!(decision.spent < decision.cap);
    assert!(
        decision
            .projected
            .is_some_and(|projected| { decision.spent.saturating_add(projected) > decision.cap })
    );
}

/// Seam: the shared root coordinator is deliberately weak and disappears
/// when both quiescent supervisors retire. Recreating the delegated child
/// must rebuild the root's durable spend before admitting another request.
///
/// MUTATION CHECK: seed the recreated coordinator with zero usage and the
/// fourth provider request opens instead of failing at projected admission.
#[tokio::test(start_paused = true)]
async fn supervisor_idle_retirement_preserves_durable_root_budget_spend() {
    let label = "budget-retirement-shared-root";
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: Usage {
                input: 100,
                output: 0,
                reasoning: 0,
                cached: 0,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
            },
        },
        FakeStep::EmitToolCall {
            call_id: "retirement-budget-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "seed durable child spend",
                "prompt": "return one report",
                "budget_tokens": 4096,
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitUsage {
            usage: Usage {
                input: 100,
                output: 0,
                reasoning: 0,
                cached: 0,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
            },
        },
        FakeStep::EmitText {
            text: "durable child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "retirement-budget-spawn".into(),
        },
        FakeStep::EmitUsage {
            usage: Usage {
                input: 180_000,
                output: 0,
                reasoning: 0,
                cached: 0,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
            },
        },
        FakeStep::EmitText {
            text: "root completed below its cap".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "recreated supervisor must not open this request".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FakeBudgetProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(crate::delegation::DelegationHandle::new(hub.clone())),
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install worker manager");

    let parent_session = SessionId::new(format!("{label}-parent-session"));
    let parent_run = RunId::new(format!("{label}-parent-run"));
    let device_id = DeviceId::new(format!("{label}-device"));
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("{label}-create"),
        request_digest: format!("{label}-create-digest"),
        request_json: format!(r#"{{"session":"{label}"}}"#),
        session_id: parent_session.clone(),
        cwd: cwd.clone(),
        provider: "openai".into(),
        model: "gpt-5.6-sol".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("{label}-created")),
        device_id: device_id.clone(),
    })
    .await
    .expect("create parent session");
    let spec = HeadlessRunSpecV1 {
        cwd,
        provider: "openai".into(),
        model: "gpt-5.6-sol".into(),
        max_output_tokens: 4096,
        effort: None,
        fast: false,
        seed: Some(968),
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        budget: RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        request_deadline_unix_ms: None,
        replay_of: None,
    };
    let request_json = serde_json::json!({
        "session_id": &parent_session,
        "text": "delegate once, then finish",
        "headless": spec,
    })
    .to_string();
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: format!("{label}-turn"),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: parent_session.clone(),
            worker_generation: store.worker_generation(),
            run_id: parent_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "delegate once, then finish".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("{label}-queued")),
            user_event_id: EventId::new(format!("{label}-user")),
            active_event_id: EventId::new(format!("{label}-active")),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept capped parent turn");
    handle.submit(accepted).await.expect("submit capped parent");
    // Registry #94: 4,010ms = the delegation path's 1s settlement tail +
    // 3s local scheduling/store allowance + one 10ms journal-poll interval.
    timeout(BUDGET_CASE_DEADLINE, async {
        loop {
            let done = store
                .read(&parent_session, 0, 1024)
                .await
                .expect("read parent")
                .iter()
                .any(|event| {
                    event.run_id.as_ref() == Some(&parent_run)
                        && event
                            .payload
                            .decode_event()
                            .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
                });
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial delegation settles inside the 1s tail plus 3.01s scheduling budget");
    let initial_parent_events = store
        .read(&parent_session, 0, 1024)
        .await
        .expect("read completed parent");
    assert!(
        initial_parent_events.iter().any(|event| {
            event.run_id.as_ref() == Some(&parent_run)
                && event
                    .payload
                    .decode_event()
                    .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
        }),
        "initial capped delegation reaches Done; requests={}; payloads={:?}",
        provider.requests().len(),
        initial_parent_events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(&parent_run))
            .map(|event| &event.payload)
            .collect::<Vec<_>>()
    );
    assert_eq!(provider.requests().len(), 3);
    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run.clone())
        .await
        .expect("delegation lookup")
        .pop()
        .expect("durable child record");
    assert_eq!(handle.supervisor_count(), 2);

    // The production TTL is five minutes. Paused-time `sleep` lets every ready
    // task run before Tokio advances the clock; `advance` plus a fixed yield
    // count did not fence the supervisors' next-loop timer arm on loaded Linux
    // runners (same determinism fix as manager_law_tests).
    let joined_before = handle.joined_supervisor_count();
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    handle
        .wait_for_joined_supervisor_count(joined_before.saturating_add(2))
        .await;
    assert_eq!(
        handle.supervisor_count(),
        0,
        "both durably quiescent supervisors retire"
    );

    let resumed_run = RunId::new(format!("{label}-recreated-child-run"));
    let resumed_json = serde_json::json!({
        "session_id": &child.child_session_id,
        "text": "recreate and debit the old root",
        "headless": &spec,
    })
    .to_string();
    let resumed = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: format!("{label}-recreated-child"),
            request_digest: blake3::hash(resumed_json.as_bytes()).to_hex().to_string(),
            request_json: resumed_json,
            session_id: child.child_session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: resumed_run.clone(),
            agent_id: Some(child.agent_id.clone()),
            branch_id: None,
            text: "recreate and debit the old root".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("{label}-recreated-queued")),
            user_event_id: EventId::new(format!("{label}-recreated-user")),
            active_event_id: EventId::new(format!("{label}-recreated-active")),
            device_id,
        })
        .await
        .expect("accept recreated child turn");
    handle
        .submit(resumed)
        .await
        .expect("recreate child supervisor transparently");
    // Registry #94: the same 4,010ms local bound contains synchronous budget
    // admission, 4s scheduling/store allowance, and one 10ms poll interval.
    timeout(BUDGET_CASE_DEADLINE, async {
        loop {
            let terminal = store
                .read(&child.child_session_id, 0, 1024)
                .await
                .expect("read recreated child")
                .iter()
                .any(|event| {
                    event.run_id.as_ref() == Some(&resumed_run)
                        && event.payload.decode_event()
                            .is_ok_and(|payload| {
                                matches!(payload, EventPayload::RunState(state) if state.is_terminal())
                            })
                });
            if terminal {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recreated admission terminalizes inside the 4.01s local budget");
    let resumed_events = store
        .read(&child.child_session_id, 0, 1024)
        .await
        .expect("read terminal recreated child");
    assert!(
        resumed_events.iter().any(|event| {
            event.run_id.as_ref() == Some(&resumed_run)
                && event.payload.decode_event().is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::RunFailed {
                            code: ErrorCode::BudgetExhausted,
                            ..
                        }
                    )
                })
        }),
        "recreated run must debit its durable root spend; requests={}; payloads={:?}",
        provider.requests().len(),
        resumed_events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(&resumed_run))
            .map(|event| &event.payload)
            .collect::<Vec<_>>()
    );
    assert!(resumed_events.iter().any(|event| {
        event.run_id.as_ref() == Some(&resumed_run)
            && event
                .payload
                .decode_event()
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Errored))
    }));
    assert_eq!(
        provider.requests().len(),
        3,
        "recreated admission binds before provider request four"
    );
    assert_eq!(handle.supervisor_count(), 1);
    let root_events = store
        .read(&parent_session, 0, 1024)
        .await
        .expect("read durable root budget");
    let exhausted = budget_fact(&root_events);
    let decision = exhausted.decision.expect("recreated projected decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert_eq!(decision.spent, 901_000);
    assert_eq!(exhausted.usage.logical_input_tokens, 180_200);
    assert!(
        decision
            .projected
            .is_some_and(|projected| decision.spent.saturating_add(projected) > decision.cap)
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[async_trait::async_trait]
impl ProviderFactory for DeadlineProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

/// Gate regression: a provider that never returns response headers is stopped
/// early enough for the durable structured failure and terminal state to reach
/// a headless client before the three-second run deadline. Dropping the open
/// future must also release its request guard.
#[tokio::test]
async fn never_opening_provider_terminalizes_before_headless_run_deadline() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let in_flight = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(NeverOpensProvider::new(Arc::clone(&in_flight)));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(DeadlineProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install worker manager");

    let session_id = SessionId::new("deadline-provider-session");
    let run_id = RunId::new("deadline-provider-run");
    let device_id = DeviceId::new("deadline-provider-device");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "deadline-provider-create".into(),
        request_digest: "deadline-provider-create-digest".into(),
        request_json: r#"{"session":"deadline-provider"}"#.into(),
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 64,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("deadline-provider-created"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create session");
    let request_deadline_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock after Unix epoch")
            .as_millis(),
    )
    .expect("test epoch milliseconds fit u64")
    .saturating_add(3_000);
    let spec = HeadlessRunSpecV1 {
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_output_tokens: 64,
        effort: None,
        fast: false,
        seed: None,
        permission_overrides: SessionPermissionOverridesV1::default(),
        trust_hooks: false,
        budget: RunBudgetV1 {
            max_time_ms: Some(5_000),
            ..RunBudgetV1::default()
        },
        request_deadline_unix_ms: Some(request_deadline_unix_ms),
        replay_of: None,
    };
    let request_json = serde_json::json!({
        "session_id": &session_id,
        "text": "never open",
        "headless": spec,
    })
    .to_string();
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "deadline-provider-turn".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "never open".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("deadline-provider-queued"),
            user_event_id: EventId::new("deadline-provider-user"),
            active_event_id: EventId::new("deadline-provider-active"),
            device_id,
        })
        .await
        .expect("accept headless turn");
    let started = Instant::now();
    manager
        .handle()
        .submit(accepted)
        .await
        .expect("submit headless turn");

    let failure = timeout(Duration::from_millis(2_900), async {
        loop {
            let events = store.read(&session_id, 0, 512).await.expect("read run");
            let failure = events
                .iter()
                .filter(|event| event.run_id.as_ref() == Some(&run_id))
                .find_map(|event| {
                    event
                        .payload
                        .decode_event()
                        .ok()
                        .and_then(|payload| match payload {
                            EventPayload::RunFailed {
                                code,
                                message,
                                retryable,
                                presentation,
                            } => Some((code, message, retryable, presentation)),
                            _ => None,
                        })
                });
            let terminal = events.iter().any(|event| {
                event.run_id.as_ref() == Some(&run_id)
                    && event
                        .payload
                        .decode_event()
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Errored))
            });
            if let Some(unexpected) = events
                .iter()
                .filter(|event| event.run_id.as_ref() == Some(&run_id))
                .filter_map(|event| event.payload.decode_event().ok())
                .find(|payload| {
                    matches!(
                        payload,
                        EventPayload::RunState(state)
                            if state.is_terminal() && *state != RunState::Errored
                    )
                })
            {
                panic!("never-opening provider reached the wrong terminal: {unexpected:?}");
            }
            if let Some(failure) = failure.filter(|_| terminal) {
                break failure;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("structured terminal arrives before the three-second deadline");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(failure.0, ErrorCode::ProviderTimeout);
    assert!(failure.1.contains("reason=deadline_exhausted"));
    assert!(!failure.2, "no full retry can fit before the deadline");
    assert_eq!(
        failure
            .3
            .as_ref()
            .expect("typed provider-timeout presentation")
            .subcode
            .as_str(),
        "provider-timeout"
    );
    assert!(
        !store
            .read(&session_id, 0, 512)
            .await
            .expect("read terminalized run")
            .iter()
            .any(|event| {
                event.run_id.as_ref() == Some(&run_id)
                    && event.payload.decode_event().is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::RunState(RunState::Waiting {
                                reason: haider_protocol::state::WaitReason::NetworkUnavailable
                            })
                        )
                    })
            }),
        "a never-opening provider on a live route must not enter WaitingForRoute"
    );
    timeout(Duration::from_millis(250), async {
        while in_flight.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out provider request is not orphaned");
    assert_eq!(provider.requests.load(Ordering::SeqCst), 1);

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
