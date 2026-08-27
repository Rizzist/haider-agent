#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::headless::{
    HeadlessRunEventPayload, HeadlessRunSpecV1, HeadlessRunUsageV1, RunBudgetDimensionV1,
    RunBudgetExhaustedV1, RunBudgetV1,
};
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::{RunState, SessionState};
use haider_store::{EventStore, Store};

use crate::turn_recovery::{
    STARTUP_HYDRATION_PAYLOAD_KINDS, interrupted_recovery_payloads_for_test,
};
use crate::worker::{
    QueuedBudgetArm, QueuedBudgetWake, budget_usage_from_envelopes_for_test, exhausted_budget,
    signal_queued_budget_change, wait_for_queued_budget_deadline_or_change,
};

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
    )
    .expect("unknown pricing cannot bypass a cost limit");
    assert_eq!(exhausted.dimension, RunBudgetDimensionV1::Cost);
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
        serde_json::from_value::<EventPayload>(event.payload.clone()),
        Ok(EventPayload::RunFailed {
            code: ErrorCode::BudgetExhausted,
            ..
        })
    )));
    assert!(matches!(
        journal
            .last()
            .and_then(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok()),
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
