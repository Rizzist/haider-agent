#![allow(clippy::unwrap_used)]
use haider_protocol::completion::*;
use haider_protocol::envelope::*;
use haider_protocol::ids::*;
use haider_protocol::{EventPayload, error::ErrorCode, state::RunState};
use serde_json::{Value, json};

fn event(seq: u64, run: Option<&str>, payload: Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("e{seq}")),
        seq,
        session_id: SessionId::new("s"),
        branch_id: None,
        run_id: run.map(RunId::new),
        agent_id: None,
        device_id: DeviceId::new("d"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}
fn monitor(seq: u64, id: &str) -> RawEnvelope {
    event(
        seq,
        None,
        json!({"type":"monitor_report_pending", "pending":{"report":{"session_id":"s", "monitor_id":"watch", "report_id":id, "action":{"follow_up":"Write one harmless marker"}}}}),
    )
}
fn transition(seq: u64, value: CompletionEvent) -> RawEnvelope {
    event(seq, None, serde_json::to_value(value).unwrap())
}
fn admission(id: &str, attempt: u32, run: &str) -> Vec<RawEnvelope> {
    vec![
        transition(
            2,
            CompletionEvent::CompletionAttempt {
                obligation_id: id.into(),
                attempt,
                worker_generation: 1,
            },
        ),
        transition(
            3,
            CompletionEvent::CompletionAdmitted {
                obligation_id: id.into(),
                attempt,
                run_id: RunId::new(run),
                accepted_seq: 3,
                worker_generation: 1,
            },
        ),
        transition(
            4,
            CompletionEvent::CompletionDelivered {
                obligation_id: id.into(),
                attempt,
            },
        ),
    ]
}
fn reduce(events: &[RawEnvelope]) -> CompletionProjection {
    let mut p = CompletionProjection::default();
    for e in events {
        p.apply(e);
    }
    p
}
fn failed(seq: u64, run: &str, code: ErrorCode, retryable: bool) -> RawEnvelope {
    event(
        seq,
        Some(run),
        serde_json::to_value(EventPayload::RunFailed {
            code,
            message: "synthetic HTTP 400".into(),
            retryable,
            presentation: None,
        })
        .unwrap(),
    )
}

#[test]
fn one_shot_delivery_and_source_removal_do_not_handle_follow_up() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        None,
        json!({"type":"monitor_report_delivered", "report_id":"r"}),
    ));
    events.push(event(
        6,
        None,
        json!({"type":"monitor_removed", "monitor_id":"watch", "reason":"one_shot_complete"}),
    ));
    let p = reduce(&events);
    assert_eq!(p.pending["monitor:r"].status, CompletionStatus::Active);
}

#[test]
fn http_400_restart_resume_keeps_id_and_changes_consuming_attempt() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(failed(5, "wake", ErrorCode::ProviderError, false));
    let p = reduce(&events);
    assert_eq!(
        p.pending["monitor:r"].park_reason,
        Some(CompletionParkReason::RequestRepair)
    );
    events.push(transition(
        6,
        CompletionEvent::CompletionResumed {
            obligation_id: "monitor:r".into(),
            attempt: 1,
        },
    ));
    events.extend(admission("monitor:r", 2, "repaired-wake"));
    let p = reduce(&events);
    assert_eq!(p.pending.len(), 1);
    assert_eq!(p.pending["monitor:r"].attempt, 2);
    assert_eq!(
        p.pending["monitor:r"].run_id,
        Some(RunId::new("repaired-wake"))
    );
}

#[test]
fn successful_turn_alone_leaves_multi_step_action_awaiting_receipt() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        Some("wake"),
        serde_json::to_value(EventPayload::RunState(RunState::Done)).unwrap(),
    ));
    assert_eq!(
        reduce(&events).pending["monitor:r"].status,
        CompletionStatus::AwaitingReceipt
    );
}

#[test]
fn pending_admission_delivery_action_ack_crash_prefixes_replay() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        Some("wake"),
        json!({"type":"synthetic_action", "marker_sha256":"fixture"}),
    ));
    events.push(transition(
        6,
        CompletionEvent::CompletionHandled {
            obligation_id: "monitor:r".into(),
            attempt: 1,
            run_id: RunId::new("wake"),
            evidence_event_ids: vec![EventId::new("e5")],
        },
    ));
    for cut in 1..events.len() {
        assert!(
            reduce(&events[..cut]).pending.contains_key("monitor:r"),
            "crash boundary {cut}"
        );
    }
    assert!(reduce(&events).pending.is_empty());
}

#[test]
fn duplicate_receipts_and_duplicate_observation_cannot_reopen_handled_action() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    let ack = transition(
        5,
        CompletionEvent::CompletionHandled {
            obligation_id: "monitor:r".into(),
            attempt: 1,
            run_id: RunId::new("wake"),
            evidence_event_ids: vec![EventId::new("action")],
        },
    );
    events.extend([ack.clone(), ack, monitor(7, "r")]);
    assert!(reduce(&events).pending.is_empty());
}

#[test]
fn stale_or_wrong_run_receipt_does_not_clear_the_current_attempt() {
    for (attempt, run) in [(0, "wake"), (1, "unrelated")] {
        let mut events = vec![monitor(1, "r")];
        events.extend(admission("monitor:r", 1, "wake"));
        events.push(transition(
            5,
            CompletionEvent::CompletionHandled {
                obligation_id: "monitor:r".into(),
                attempt,
                run_id: RunId::new(run),
                evidence_event_ids: vec![EventId::new("action")],
            },
        ));
        assert!(reduce(&events).pending.contains_key("monitor:r"));
    }
}

#[test]
fn two_coalesced_wakes_in_one_busy_run_need_separate_receipts() {
    let mut events = vec![monitor(1, "r1"), monitor(2, "r2")];
    events.extend(admission("monitor:r1", 1, "busy"));
    events.extend(admission("monitor:r2", 1, "busy"));
    events.push(transition(
        8,
        CompletionEvent::CompletionHandled {
            obligation_id: "monitor:r1".into(),
            attempt: 1,
            run_id: RunId::new("busy"),
            evidence_event_ids: vec![EventId::new("action")],
        },
    ));
    let p = reduce(&events);
    assert_eq!(p.pending.len(), 1);
    assert!(p.pending.contains_key("monitor:r2"));
}

#[test]
fn budget_and_provider_failures_park_without_clearing_obligation() {
    for (code, retryable, reason) in [
        (
            ErrorCode::RequestBudgetExceeded,
            false,
            CompletionParkReason::Budget,
        ),
        (
            ErrorCode::BudgetExhausted,
            false,
            CompletionParkReason::Budget,
        ),
        (
            ErrorCode::ProviderError,
            true,
            CompletionParkReason::ProviderRecovery,
        ),
    ] {
        let mut events = vec![monitor(1, "r")];
        events.extend(admission("monitor:r", 1, "wake"));
        events.push(failed(5, "wake", code, retryable));
        assert_eq!(
            reduce(&events).pending["monitor:r"].park_reason,
            Some(reason)
        );
    }
}

#[test]
fn explicit_watch_removal_dismisses_follow_up_but_timeout_does_not() {
    for reason in ["removed", "timed_out"] {
        let events = vec![
            monitor(1, "r"),
            event(
                2,
                None,
                json!({"type":"monitor_removed","monitor_id":"watch","reason":reason}),
            ),
        ];
        assert_eq!(reduce(&events).pending.is_empty(), reason == "removed");
    }
}

#[test]
fn failure_between_admission_and_delivery_is_retained_when_receipt_arrives_late() {
    let mut events = vec![monitor(1, "r")];
    events.push(transition(
        2,
        CompletionEvent::CompletionAttempt {
            obligation_id: "monitor:r".into(),
            attempt: 1,
            worker_generation: 1,
        },
    ));
    events.push(failed(3, "wake", ErrorCode::ProviderError, false));
    events.extend(admission("monitor:r", 1, "wake").into_iter().skip(1));
    assert_eq!(
        reduce(&events).pending["monitor:r"].park_reason,
        Some(CompletionParkReason::RequestRepair)
    );
}

#[test]
fn independent_fork_cannot_wake_copied_obligations() {
    let events = vec![
        monitor(1, "r"),
        event(2, None, json!({"type":"session_forked"})),
    ];
    assert!(reduce(&events).pending.is_empty());
    let mut copied = monitor(1, "r");
    copied.session_id = SessionId::new("child");
    assert!(reduce(&[copied]).pending.is_empty());
}

#[test]
fn task_completion_is_visible_without_auto_admission() {
    let task = haider_protocol::task::TaskCompleted {
        completion_consumer: None,
        task: TaskId::new("t"),
        name: "fixture".into(),
        state: haider_protocol::task::TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 1,
        output_bytes: 0,
        output_sha256: None,
        tail: String::new(),
        artifact: None,
        full_output_unavailable: false,
        truncated: false,
        delivery: haider_protocol::task::TaskCompletionDelivery::DeliveredQueued,
        workspace_mutation: None,
    };
    let p = reduce(&[event(
        1,
        Some("source-run"),
        task.to_payload_value().unwrap(),
    )]);
    assert_eq!(p.pending["task:t"].attempt, 0);
    assert_eq!(p.pending["task:t"].run_id, None);
}

#[test]
fn additive_completion_journal_goldens_round_trip_json_and_messagepack() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/completion-v1.json")).unwrap();
    for fixture in fixtures {
        let typed: CompletionEvent = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), fixture);
        let packed = rmp_serde::to_vec_named(&typed).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<CompletionEvent>(&packed).unwrap(),
            typed
        );
    }
}

#[test]
fn answered_menu_follow_up_survives_failure_without_reopening_question() {
    use haider_protocol::menu::*;
    let menu = Menu {
        id: MenuId::new("m"),
        kind: MenuKind::Question,
        title: "Choose".into(),
        body: vec![],
        options: vec![],
        blocking: true,
        scope: MenuScope::Session,
        origin: "request_input".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let answer = MenuAnswer {
        menu: MenuId::new("m"),
        option_key: None,
        option_index: 0,
        value: Some("yes".into()),
        via: AnswerVia::Rpc,
    };
    let events = vec![
        event(
            1,
            Some("wake"),
            serde_json::to_value(EventPayload::MenuOpened(menu)).unwrap(),
        ),
        event(
            2,
            Some("wake"),
            serde_json::to_value(EventPayload::MenuAnswered(answer.clone())).unwrap(),
        ),
        failed(3, "wake", ErrorCode::ProviderError, false),
        event(
            4,
            Some("wake"),
            serde_json::to_value(EventPayload::MenuAnswered(answer)).unwrap(),
        ),
    ];
    let p = reduce(&events);
    assert_eq!(p.pending.len(), 1);
    assert_eq!(p.pending["menu:m"].status, CompletionStatus::Parked);
    assert_eq!(p.pending["menu:m"].created_seq, 2);
}

#[test]
fn native_run_retry_rebinds_the_obligation_and_counts_a_new_attempt() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(failed(5, "wake", ErrorCode::ProviderError, false));
    events.push(event(
        6,
        Some("retry"),
        serde_json::to_value(haider_protocol::retry::RunRetryEventPayload::RunRetried {
            failed_run_id: RunId::new("wake"),
            prompt_run_id: RunId::new("wake"),
            user_seq: 3,
        })
        .unwrap(),
    ));
    let p = reduce(&events);
    let obligation = &p.pending["monitor:r"];
    assert_eq!(obligation.attempt, 2);
    assert_eq!(obligation.run_id, Some(RunId::new("retry")));
    assert_eq!(obligation.accepted_seq, Some(6));
    assert_eq!(obligation.status, CompletionStatus::Active);
}

#[test]
fn task_follow_up_uses_the_consuming_branch_after_steer() {
    let task = haider_protocol::task::TaskCompleted {
        completion_consumer: Some(haider_protocol::task::TaskCompletionConsumer {
            run_id: RunId::new("active"),
            branch_id: Some(BranchId::new("target-branch")),
        }),
        task: TaskId::new("t"),
        name: "fixture".into(),
        state: haider_protocol::task::TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 1,
        output_bytes: 0,
        output_sha256: None,
        tail: String::new(),
        artifact: None,
        full_output_unavailable: false,
        truncated: false,
        delivery: haider_protocol::task::TaskCompletionDelivery::DeliveredSteer,
        workspace_mutation: None,
    };
    let payload = task.to_payload_value().unwrap();
    let roundtrip: haider_protocol::task::TaskEventPayload =
        serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(roundtrip.to_payload_value().unwrap(), payload);
    let mut source = event(1, Some("source-run"), payload);
    source.branch_id = Some(BranchId::new("source-branch"));
    let p = reduce(&[source]);
    assert_eq!(
        p.pending["task:t"].branch_id,
        Some(BranchId::new("target-branch"))
    );
    assert_eq!(p.pending["task:t"].run_id, Some(RunId::new("active")));
}

#[test]
fn typed_restart_interruption_rearms_but_provider_failure_stays_parked() {
    use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        Some("wake"),
        serde_json::to_value(EventPayload::RunFailed {
            code: ErrorCode::Internal,
            message: "run was interrupted by daemon restart".into(),
            retryable: true,
            presentation: Some(ErrorPresentation::new(
                "run-recovery-interrupted",
                "Interrupted run could not resume",
                "synthetic restart",
                ErrorScope::Turn,
                [ErrorAction::RetryFresh],
            )),
        })
        .unwrap(),
    ));
    assert_eq!(
        reduce(&events).pending["monitor:r"].park_reason,
        Some(CompletionParkReason::Interrupted)
    );
    events.push(failed(6, "wake", ErrorCode::ProviderError, true));
    assert_eq!(
        reduce(&events).pending["monitor:r"].park_reason,
        Some(CompletionParkReason::ProviderRecovery)
    );
}

#[test]
fn nonretryable_quota_parks_for_account_recovery_without_resetting_attempts() {
    use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        Some("wake"),
        serde_json::to_value(EventPayload::RunFailed {
            code: ErrorCode::ProviderError,
            message: "synthetic quota exhausted".into(),
            retryable: false,
            presentation: Some(ErrorPresentation::new(
                "quota-exhausted",
                "Credits or quota exhausted",
                "fixture account recovery",
                ErrorScope::Account,
                [ErrorAction::TopUp, ErrorAction::SwitchAccount],
            )),
        })
        .unwrap(),
    ));
    let pending = &reduce(&events).pending["monitor:r"];
    assert_eq!(pending.status, CompletionStatus::Parked);
    assert_eq!(
        pending.park_reason,
        Some(CompletionParkReason::ProviderRecovery)
    );
    assert_eq!(pending.attempt, 1);
}

#[test]
fn cancelled_wake_retains_the_obligation_until_explicit_dismissal() {
    let mut events = vec![monitor(1, "r")];
    events.extend(admission("monitor:r", 1, "wake"));
    events.push(event(
        5,
        Some("wake"),
        serde_json::to_value(EventPayload::RunState(RunState::Cancelled)).unwrap(),
    ));
    let interrupted = reduce(&events);
    assert_eq!(
        interrupted.pending["monitor:r"].status,
        CompletionStatus::Parked
    );
    assert_eq!(
        interrupted.pending["monitor:r"].park_reason,
        Some(CompletionParkReason::Interrupted)
    );
    events.push(transition(
        6,
        CompletionEvent::CompletionDismissed {
            obligation_id: "monitor:r".into(),
            attempt: 1,
        },
    ));
    let dismissed = reduce(&events);
    assert!(dismissed.pending.is_empty());
    assert!(dismissed.is_settled("monitor:r"));
}
