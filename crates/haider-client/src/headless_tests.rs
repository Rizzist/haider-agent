#![allow(clippy::expect_used)]

use super::{
    BufferedWireFrames, EnsureOptions, HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES, HeadlessAttachment,
    HeadlessEventLedgerWriter, HeadlessRunError, HeadlessRunEventStorage, HeadlessSessionConfig,
    headless_submit_body, load_attachment, load_pdf_attachment, normalize_session_config_features,
};
use haider_rpc::haider_protocol::envelope::RawEnvelope;
use haider_rpc::haider_protocol::ids::{RunId, SessionId};
use haider_rpc::{CommandId, RequestBody};

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
fn empty_replay_buffer_is_lazy_even_when_read_or_cleared() {
    let mut buffered = BufferedWireFrames::new();
    assert!(buffered.file.is_none());
    assert!(buffered.cleanup.is_none());
    assert_eq!(buffered.reader().expect("empty reader").count(), 0);
    buffered.clear().expect("clear empty replay buffer");
    assert!(buffered.file.is_none());
    assert!(buffered.cleanup.is_none());
}

/// MUTATION CHECK: restore the client-only `session_account_select_v1`
/// requirement and its bare `missing_feature` failure. This assertion then
/// receives `Ok(())` instead of the actionable pre-connect error.
#[test]
fn unsupported_account_selection_names_model_selector_workaround() {
    let mut ensure = EnsureOptions::default();
    let config = HeadlessSessionConfig {
        account: Some("work".into()),
        ..HeadlessSessionConfig::default()
    };
    let error = normalize_session_config_features(&mut ensure, &config)
        .expect_err("account selection is not a daemon capability");
    assert!(matches!(
        &error,
        HeadlessRunError::Bootstrap {
            stage: "session config",
            code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT,
            retryable: false,
            ..
        }
    ));
    assert!(
        error.to_string().contains("--model provider/model"),
        "the error must name the implemented routing control: {error}"
    );
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
