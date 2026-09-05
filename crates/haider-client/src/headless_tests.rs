#![allow(clippy::expect_used)]

use super::{
    ApplyStatus, BufferedWireFrames, EnsureOptions, HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES,
    HeadlessAttachment, HeadlessEvent, HeadlessEventLedgerWriter, HeadlessEventMode,
    HeadlessEventOutput, HeadlessFailureCode, HeadlessInterrupt, HeadlessOutcome, HeadlessReducer,
    HeadlessRunError, HeadlessRunEventStorage, HeadlessRunFailure, HeadlessSessionConfig,
    HeadlessTerminalKind, headless_submit_body, load_attachment, load_pdf_attachment,
    normalize_session_config_features, terminal_kind, try_take_pending_interrupt,
};
use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::envelope::RawEnvelope;
use haider_rpc::haider_protocol::error::ErrorCode;
use haider_rpc::haider_protocol::headless::{HeadlessRunEventPayload, RunDeadlineExceededV1};
use haider_rpc::haider_protocol::ids::{RunId, SessionId};
use haider_rpc::haider_protocol::state::RunState;
use haider_rpc::{CommandId, RequestBody};

#[test]
fn pending_second_interrupt_is_consumed_before_terminal_drain() {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut interrupts = Some(receiver);
    assert!(!try_take_pending_interrupt(&mut interrupts));

    sender
        .send(HeadlessInterrupt::ExitImmediately)
        .expect("interrupt receiver remains open");
    assert!(try_take_pending_interrupt(&mut interrupts));

    drop(sender);
    assert!(!try_take_pending_interrupt(&mut interrupts));
    assert!(interrupts.is_none());
}

#[cfg(unix)]
use super::reap_owned_daemon;
#[cfg(unix)]
use crate::spawn::DaemonOwnershipToken;
#[cfg(unix)]
use std::io::BufRead as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant as StdInstant};
#[cfg(unix)]
use tokio::time::Instant;

fn spool_test_envelope(seq: u64, payload: serde_json::Value) -> RawEnvelope {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "event_id": format!("spool-event-{seq}"),
        "seq": seq,
        "session_id": "spool-session",
        "run_id": "spool-run",
        "device_id": "spool-device",
        "authority_epoch": 1,
        "worker_generation": 1,
        "committed_at_ms": seq,
        "render": {"ui": true, "durable": true, "prompt": "omit"},
        "payload": payload,
    }))
    .expect("raw spool envelope")
}

#[test]
fn memory_and_forced_spool_ledgers_serialize_to_identical_bytes() {
    let run_id = RunId::new("spool-run");
    let envelopes = vec![
        spool_test_envelope(1, serde_json::json!({"type": "one", "value": 1})),
        spool_test_envelope(2, serde_json::json!({"type": "two", "value": 2})),
    ];
    let mut memory = HeadlessEventLedgerWriter::new(false);
    let mut spool = HeadlessEventLedgerWriter::new(true);
    for envelope in &envelopes {
        memory.record(envelope);
        spool.record(envelope);
    }
    let memory = memory
        .finish(run_id.clone(), envelopes.len())
        .expect("memory ledger");
    let spool = spool
        .finish(run_id, envelopes.len())
        .expect("forced spool ledger");

    assert!(matches!(
        &memory.storage,
        HeadlessRunEventStorage::Memory(_)
    ));
    assert!(matches!(&spool.storage, HeadlessRunEventStorage::Spool(_)));
    assert_eq!(
        serde_json::to_vec(&memory).expect("serialize memory ledger"),
        serde_json::to_vec(&spool).expect("serialize spool ledger")
    );
}

#[test]
fn threshold_spill_moves_the_complete_prefix_and_preserves_order() {
    let envelopes = vec![
        spool_test_envelope(1, serde_json::json!({"type": "before"})),
        spool_test_envelope(
            2,
            serde_json::json!({
                "type": "large",
                "value": "x".repeat(HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES),
            }),
        ),
        spool_test_envelope(3, serde_json::json!({"type": "after"})),
    ];
    let mut writer = HeadlessEventLedgerWriter::new(false);
    for envelope in &envelopes {
        writer.record(envelope);
    }
    let ledger = writer
        .finish(RunId::new("spool-run"), envelopes.len())
        .expect("threshold-spilled ledger");
    assert!(matches!(&ledger.storage, HeadlessRunEventStorage::Spool(_)));
    let replayed = ledger
        .iter()
        .expect("spool reader")
        .collect::<Result<Vec<_>, _>>()
        .expect("complete spool");
    assert_eq!(replayed, envelopes);
}

#[test]
fn stream_without_result_ledger_forwards_once_and_returns_an_empty_ledger() {
    let envelope = spool_test_envelope(1, serde_json::json!({"type": "future_event"}));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut output = HeadlessEventOutput::new(sender, HeadlessEventMode::StreamWithoutResultLedger);
    output.emit_envelope(envelope.clone(), true);
    let result_events = output
        .finish(RunId::new("spool-run"), 1)
        .expect("stream-only output finishes without a retained clone");

    assert!(result_events.is_empty());
    assert_eq!(
        receiver.try_recv().expect("streamed envelope"),
        HeadlessEvent::Envelope(Box::new(envelope))
    );
    assert!(receiver.try_recv().is_err(), "envelope is forwarded once");
}

/// MUTATION CHECK: return directly from the first reap timeout. The TERM-
/// ignoring owned child then remains alive and its process group survives the
/// assertion instead of being force-terminated and unconditionally reaped.
#[cfg(unix)]
#[tokio::test]
async fn timed_out_owned_daemon_reap_terminates_and_reaps_the_process_group() {
    let mut child = Command::new("/bin/sh");
    child
        .arg("-c")
        .arg("trap '' TERM; printf 'ready\\n'; while :; do sleep 1; done")
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = child.spawn().expect("spawn TERM-ignoring owned child");
    let pid = child.id();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().expect("owned child stdout"))
        .read_line(&mut ready)
        .expect("owned child readiness");
    assert_eq!(ready, "ready\n");

    let ownership = DaemonOwnershipToken {
        child,
        authenticated_pid: pid,
        instance_id: "timed-out-reap-instance".into(),
        daemon_generation: 1,
        _liveness: None,
    };
    let error = reap_owned_daemon(ownership, Instant::now())
        .await
        .expect_err("deadline escalation remains a truthful teardown error");
    assert!(
        error.to_string().contains("terminated before final reap"),
        "unexpected teardown error: {error}"
    );

    let group = haider_platform::process_group(Some(pid)).expect("owned process group");
    let proof_deadline = StdInstant::now() + Duration::from_secs(1);
    loop {
        let alive = haider_platform::process_group_exists(group)
            .expect("probe force-terminated process group");
        if !alive {
            break;
        }
        assert!(
            StdInstant::now() < proof_deadline,
            "owned process group {pid} survived timed-out reap escalation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn empty_replay_buffer_is_lazy_even_when_read_or_cleared() {
    let mut buffered = BufferedWireFrames::new();
    assert!(buffered.file.is_none());
    assert!(buffered.cleanup.is_none());
    assert_eq!(buffered.reader().expect("empty reader").count(), 0);
    buffered.clear().expect("clear empty replay buffer");
    assert!(buffered.file.is_none());
    assert!(buffered.cleanup.is_none());
}

/// MUTATION CHECK: omit the account-selection feature precondition. The
/// daemon could then accept a run without understanding its routing pin.
#[test]
fn account_selection_requires_the_daemon_contract() {
    let mut ensure = EnsureOptions::default();
    let config = HeadlessSessionConfig {
        account: Some("work".into()),
        ..HeadlessSessionConfig::default()
    };
    normalize_session_config_features(&mut ensure, &config).expect("account selection supported");
    assert!(
        ensure
            .required_features
            .contains(haider_rpc::FEATURE_SESSION_ACCOUNT_SELECT_V1)
    );
}

/// MUTATION CHECK: collapse provider timeout into caller timeout, or merge a
/// budget/cancellation/failure into one generic terminal. The six stable kinds
/// below then stop being distinct.
#[test]
fn terminal_kind_vocabulary_is_distinct_and_provider_timeout_stays_provider_owned() {
    let failure = |code| HeadlessRunFailure {
        code: HeadlessFailureCode::Run(code),
        message: "fixture".into(),
        retryable: false,
        presentation: None,
    };
    assert_eq!(
        terminal_kind(HeadlessOutcome::Done, None),
        HeadlessTerminalKind::Success
    );
    assert_eq!(
        terminal_kind(HeadlessOutcome::Cancelled, None),
        HeadlessTerminalKind::Cancellation
    );
    assert_eq!(
        terminal_kind(HeadlessOutcome::Timeout, None),
        HeadlessTerminalKind::Timeout
    );
    assert_eq!(
        terminal_kind(
            HeadlessOutcome::Errored,
            Some(&failure(ErrorCode::StoreFull))
        ),
        HeadlessTerminalKind::Failure
    );
    assert_eq!(
        terminal_kind(
            HeadlessOutcome::Errored,
            Some(&failure(ErrorCode::BudgetExhausted))
        ),
        HeadlessTerminalKind::Budget
    );
    for code in [ErrorCode::ProviderError, ErrorCode::ProviderTimeout] {
        assert_eq!(
            terminal_kind(HeadlessOutcome::Errored, Some(&failure(code))),
            HeadlessTerminalKind::ProviderError
        );
    }
}

/// MUTATION CHECK: discard the durable deadline cause or choose the exit by
/// provider phase. The adjacent ProviderTimeout would then project as a
/// provider error instead of the one caller-timeout terminal.
#[tokio::test]
async fn durable_request_deadline_reason_wins_terminal_race_as_timeout() {
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let output = HeadlessEventOutput::new(sender, HeadlessEventMode::Summary);
    let mut reducer = HeadlessReducer::new(SessionId::new("spool-session"), output);
    reducer.run_id = Some(RunId::new("spool-run"));

    let deadline = HeadlessRunEventPayload::RunDeadlineExceeded(RunDeadlineExceededV1 {
        deadline_unix_ms: 42,
    });
    assert_eq!(
        reducer
            .apply(spool_test_envelope(
                1,
                deadline
                    .to_payload_value()
                    .expect("deadline fact serializes"),
            ))
            .await,
        ApplyStatus::Applied
    );
    assert_eq!(
        reducer
            .apply(spool_test_envelope(
                2,
                serde_json::to_value(EventPayload::RunFailed {
                    code: ErrorCode::ProviderTimeout,
                    message: "response deadline elapsed".into(),
                    retryable: true,
                    presentation: None,
                })
                .expect("run failure serializes"),
            ))
            .await,
        ApplyStatus::Applied
    );
    assert_eq!(
        reducer
            .apply(spool_test_envelope(
                3,
                serde_json::to_value(EventPayload::RunState(RunState::Errored))
                    .expect("terminal state serializes"),
            ))
            .await,
        ApplyStatus::Applied
    );

    let terminal = reducer.terminal.expect("typed natural terminal");
    assert_eq!(terminal.outcome, HeadlessOutcome::Timeout);
    assert_eq!(terminal.seq, 3);
    assert!(matches!(
        terminal.failure,
        Some(HeadlessRunFailure {
            code: HeadlessFailureCode::Timeout,
            ..
        })
    ));
}

/// MUTATION CHECK: ignore the run-scoped trust bit or change ordinary turn
/// bytes. Expected RUNTIME failure: the concrete request variant observed by
/// this production builder is wrong.
#[test]
fn submit_builder_selects_hook_trust_without_changing_ordinary_turns() {
    let ordinary = headless_submit_body(
        false,
        CommandId::new("ordinary-command"),
        SessionId::new("session"),
        7,
        "ordinary".into(),
        Vec::new(),
    );
    assert!(matches!(ordinary, RequestBody::TurnSubmit { .. }));

    let trusted = headless_submit_body(
        true,
        CommandId::new("trusted-command"),
        SessionId::new("session"),
        7,
        "trusted".into(),
        Vec::new(),
    );
    assert!(matches!(
        trusted,
        RequestBody::TurnSubmitWithHookTrust {
            branch_id: None,
            ..
        }
    ));
}

fn pdf_fixture(pages: u32) -> Vec<u8> {
    let mut pdf = String::from("%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let kids = (0..pages)
        .map(|index| format!("{} 0 R", index + 3))
        .collect::<Vec<_>>()
        .join(" ");
    pdf.push_str(&format!(
        "2 0 obj\n<< /Type /Pages /Count {pages} /Kids [{kids}] >>\nendobj\n"
    ));
    for index in 0..pages {
        pdf.push_str(&format!(
            "{} 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n",
            index + 3
        ));
    }
    pdf.push_str("trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    pdf.into_bytes()
}

#[test]
fn pdf_loader_accepts_case_insensitive_extension_and_records_pages() {
    let directory = tempfile::tempdir().expect("PDF tempdir");
    let path = directory.path().join("Report.PDF");
    std::fs::write(&path, pdf_fixture(12)).expect("write PDF");
    let loaded = load_attachment(&path).expect("PDF loads through shared ingress");
    let HeadlessAttachment::Pdf(pdf) = loaded else {
        panic!("PDF extension must select the PDF lane");
    };
    assert_eq!(pdf.name, "Report.PDF");
    assert_eq!(pdf.pages, 12);

    let large_path = directory.path().join("Large.pdf");
    let mut large_pdf = pdf_fixture(1);
    large_pdf.resize(6 * 1024 * 1024, b' ');
    std::fs::write(&large_path, large_pdf).expect("write PDF above image cap");
    assert!(matches!(
        load_attachment(&large_path),
        Ok(HeadlessAttachment::Pdf(_))
    ));
}

#[test]
fn pdf_loader_page_and_byte_caps_are_typed_presentations() {
    let directory = tempfile::tempdir().expect("PDF tempdir");
    let too_many = directory.path().join("too-many.pdf");
    std::fs::write(&too_many, pdf_fixture(haider_pdf::MAX_PDF_PAGES + 1))
        .expect("write page-heavy PDF");
    let error = load_pdf_attachment(&too_many).expect_err("page cap rejects");
    assert!(matches!(
        error,
        HeadlessRunError::Attachment { ref code, ref presentation, .. }
            if code == "pdf-too-many-pages"
                && presentation.subcode.as_str() == "pdf-too-many-pages"
    ));

    let too_large = directory.path().join("too-large.pdf");
    let file = std::fs::File::create(&too_large).expect("create sparse PDF");
    file.set_len((haider_pdf::MAX_PDF_BYTES + 1) as u64)
        .expect("size sparse PDF");
    let error = load_pdf_attachment(&too_large).expect_err("byte cap rejects");
    assert!(matches!(
        error,
        HeadlessRunError::Attachment { ref code, ref presentation, .. }
            if code == "pdf-too-large" && presentation.subcode.as_str() == "pdf-too-large"
    ));
}

fn resumable_source() -> (
    super::HeadlessRunStatus,
    haider_rpc::haider_protocol::request_budget::RequestBudgetStatusV1,
) {
    use haider_rpc::haider_protocol::request_budget::{
        RequestBudgetContinuationV1, RequestBudgetPhaseV1, RequestBudgetStatusV1, RequestBudgetV1,
    };
    let source: super::HeadlessRunStatus = serde_json::from_value(serde_json::json!({
        "session_id": "resume-session", "run_id": "resume-source", "worker_generation": 7,
        "state": {"state": "errored"}, "head_seq": 2, "terminal_seq": 2, "budget_exhausted": null,
        "spec": {"cwd": "/tmp", "provider": "fake", "model": "fake-model", "max_output_tokens": 4096,
            "permission_overrides": {}, "budget": {"max_time_ms": 30000}}
    })).expect("source status");
    let checkpoint = RequestBudgetStatusV1 {
        used: 64,
        budget: RequestBudgetV1::default(),
        phase: RequestBudgetPhaseV1::HardBound,
        continuation: RequestBudgetContinuationV1 {
            session_id: source.session_id.clone(),
            run_id: source.run_id.clone(),
            branch_id: None,
            agent_id: None,
        },
    };
    (source, checkpoint)
}

#[test]
fn resume_rejects_active_and_nonbudget_sources() {
    let (mut source, checkpoint) = resumable_source();
    assert!(
        matches!(super::validate_resume_checkpoint(&source, None), Err(HeadlessRunError::Rpc { code, .. }) if code == "continuation_unavailable")
    );
    source.state = RunState::Streaming;
    source.terminal_seq = None;
    assert!(
        matches!(super::validate_resume_checkpoint(&source, Some(&checkpoint)), Err(HeadlessRunError::Rpc { code, .. }) if code == "continuation_active")
    );
}

#[test]
fn resume_accepts_terminal_soft_and_hard_checkpoints_and_checks_scope() {
    use haider_rpc::haider_protocol::request_budget::RequestBudgetPhaseV1;
    let (mut source, mut checkpoint) = resumable_source();
    super::validate_resume_checkpoint(&source, Some(&checkpoint)).expect("hard-bound continuation");
    source.state = RunState::Done;
    checkpoint.phase = RequestBudgetPhaseV1::SoftBound;
    checkpoint.used = 32;
    super::validate_resume_checkpoint(&source, Some(&checkpoint)).expect("soft checkpoint finish");
    checkpoint.continuation.run_id = RunId::new("another-run");
    assert!(super::validate_resume_checkpoint(&source, Some(&checkpoint)).is_err());
    checkpoint.continuation.run_id = source.run_id.clone();
    checkpoint.continuation.agent_id =
        Some(haider_rpc::haider_protocol::ids::AgentId::new("child"));
    assert!(
        matches!(super::validate_resume_checkpoint(&source, Some(&checkpoint)), Err(HeadlessRunError::Rpc { code, .. }) if code == "continuation_scope_unsupported")
    );
}

#[test]
fn resume_inherits_budgets_and_applies_only_explicit_new_caps() {
    use haider_rpc::haider_protocol::headless::RunBudgetV1;
    use haider_rpc::haider_protocol::request_budget::RequestBudgetV1;
    let mut original = RunBudgetV1 {
        max_time_ms: Some(30_000),
        max_tokens: Some(8000),
        max_cost_microusd: Some(90_000),
        request_budget: Some(RequestBudgetV1::default()),
    };
    let overrides = RunBudgetV1 {
        request_budget: Some(RequestBudgetV1 {
            tranche: 48,
            hard_cap: 96,
        }),
        ..RunBudgetV1::default()
    };
    super::merge_resume_budget(&mut original, &overrides);
    assert_eq!(original.max_time_ms, Some(30_000));
    assert_eq!(original.max_tokens, Some(8000));
    assert_eq!(original.max_cost_microusd, Some(90_000));
    assert_eq!(original.request_budget, overrides.request_budget);
}

#[tokio::test]
async fn journalview_private_summary_does_not_replace_the_live_response() {
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let output = HeadlessEventOutput::new(sender, HeadlessEventMode::Summary);
    let mut reducer = HeadlessReducer::new(SessionId::new("spool-session"), output);
    reducer.run_id = Some(RunId::new("spool-run"));
    reducer
        .apply(spool_test_envelope(
            1,
            serde_json::json!({
                "type":"item", "event":"completed", "item_id":"summary",
                "item":{"item":"agent_message", "text":"private compaction summary"},
                "provider_purpose":"compaction",
            }),
        ))
        .await;
    assert!(reducer.response.is_none());
}
