#![allow(clippy::expect_used)]

mod common;

use haider_protocol::ids::SessionId;
use haider_rpc::{AttachmentId, ERROR_CODE_BUSY, SessionSummary};
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;

/// E7 visual law: the local-only rejection is a MATTER-OF-FACT status
/// line — the typed protocol admission message in one quiet flash, gone on
/// the next keystroke. It never latches the persistent diagnostic banner:
/// asking for an unsupported lane is not an error condition.
#[test]
fn e7_legacy_peers_command_is_a_typed_local_only_rejection() {
    let mut model = common::launcher_model();
    common::submit(&mut model, "/peers");

    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("not supported — Haider runs local-only")),
        "the typed admission message rides the flash line"
    );
    assert!(
        model.command_diagnostic.is_none(),
        "a matter-of-fact rejection never wears the persistent banner"
    );
}

/// E8 busy mutation law: changing the command id, removing the deadline, or
/// dropping the three-issue cap breaks the exact sequence below.
#[test]
fn busy_same_command_id_retries_visibly_and_stops_after_three_issues() {
    let session = SessionId::new("busy-session");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    let mut driver = LiveDriver::new("e8-busy");
    let initial = driver.handle_request(
        &mut model,
        AppRequest::Rename {
            session,
            title: "bounded".into(),
        },
    );
    let LiveCommand::Rename { command_id, .. } = &initial[0] else {
        panic!("rename command")
    };
    let command_id = command_id.clone();
    let base = std::time::Instant::now();

    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id.clone()),
            code: ERROR_CODE_BUSY.into(),
            message: "busy".into(),
            retryable: true,
            presentation: None,
        }),
        base,
    );
    assert!(pass.commands.is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("2/3"))
    );

    // Deadline pin: a pass BEFORE the 250ms retry deadline must reissue
    // nothing — dropping the deadline filter fires the retry on the very
    // next pass, which the original sequence (passes only AT the deadline)
    // could not observe (mutation survived unpinned).
    let premature = live_pass(
        &mut driver,
        &mut model,
        None,
        base + std::time::Duration::from_millis(100),
    );
    assert!(premature.commands.is_empty());

    let second = live_pass(
        &mut driver,
        &mut model,
        None,
        base + std::time::Duration::from_millis(250),
    );
    assert!(matches!(
        second.commands.as_slice(),
        [LiveCommand::Rename { command_id: resent, .. }] if resent == &command_id
    ));
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id.clone()),
            code: ERROR_CODE_BUSY.into(),
            message: "still busy".into(),
            retryable: true,
            presentation: None,
        }),
        base + std::time::Duration::from_millis(250),
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("3/3"))
    );
    let third = live_pass(
        &mut driver,
        &mut model,
        None,
        base + std::time::Duration::from_millis(500),
    );
    assert!(matches!(
        third.commands.as_slice(),
        [LiveCommand::Rename { command_id: resent, .. }] if resent == &command_id
    ));
    let exhausted = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: ERROR_CODE_BUSY.into(),
            message: "still busy".into(),
            retryable: true,
            presentation: None,
        }),
        base + std::time::Duration::from_millis(500),
    );
    assert!(exhausted.commands.is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("bound exhausted"))
    );
    assert_eq!(
        model
            .command_diagnostic
            .as_ref()
            .map(|presentation| presentation.subcode.as_str()),
        Some("busy-retry-exhausted")
    );
    assert!(driver.next_deadline().is_none());
}

#[test]
fn link_supervisor_restart_is_visible_and_terminal_failure_is_persistent() {
    let mut model = AppModel::new();
    let mut driver = LiveDriver::new("e5-link-supervisor");
    driver.apply(
        &mut model,
        LiveReply::SupervisorRestarting {
            component: "link",
            attempt: 2,
            max: 2,
        },
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("attempt 2/2"))
    );
    driver.apply(
        &mut model,
        LiveReply::SupervisorFailed {
            component: "link",
            reason: "unexpected task death".into(),
        },
    );
    let diagnostic = model
        .supervisor_diagnostic
        .as_ref()
        .expect("persistent supervisor card");
    assert_eq!(diagnostic.subcode.as_str(), "supervisor-unavailable");
    assert!(diagnostic.detail.contains("unexpected task death"));
}

#[test]
fn store_unwritable_banner_state_is_persistent_until_healthy_edge() {
    let mut model = AppModel::new();
    let mut driver = LiveDriver::new("e5-banner");
    let presentation = haider_protocol::error::ErrorPresentation::new(
        "store-full",
        "Store unwritable",
        "Store unwritable — profile disk is full",
        haider_protocol::error::ErrorScope::Profile,
        [haider_protocol::error::ErrorAction::Retry],
    );
    driver.apply(
        &mut model,
        LiveReply::ProfileDiagnostic {
            card: Some(haider_protocol::menu::ErrorRecoveryCardKind::StoreUnwritable),
            presentation: Some(presentation.clone()),
            failed_write_ids: vec!["event-5".into()],
        },
    );
    assert_eq!(model.profile_diagnostic, Some(presentation));
    driver.apply(&mut model, LiveReply::Reconnected);
    assert!(
        model.profile_diagnostic.is_some(),
        "reconnect cannot clear it"
    );
    driver.apply(
        &mut model,
        LiveReply::ProfileDiagnostic {
            card: None,
            presentation: None,
            failed_write_ids: Vec::new(),
        },
    );
    assert!(model.profile_diagnostic.is_none());
}

#[test]
fn post_start_microphone_failure_uses_typed_voice_status_and_preserves_ghost() {
    let mut model = AppModel::new();
    model.talk.phase = haider_tui::talk::TalkPhase::Listening;
    model.talk.ghost = "unsent words".into();
    model.handle_talk(haider_tui::talk::TalkEvent::Health {
        generation: model.talk.generation,
        health: haider_stt::capture::CaptureHealth::Failed {
            error: "device vanished".into(),
        },
    });
    assert!(model.composer.text().contains("unsent words"));
    let diagnostic = model.voice_diagnostic.expect("typed persistent card");
    assert_eq!(diagnostic.subcode.as_str(), "microphone-unavailable");
    assert!(diagnostic.detail.contains("device vanished"));
}

#[test]
fn sustained_unknown_payloads_emit_one_durable_compatibility_diagnostic() {
    let session = SessionId::new("future-session");
    let attachment = AttachmentId::new("future-attachment");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    let mut driver = LiveDriver::new("compat");
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![SessionSummary {
                session_id: session.clone(),
                head_seq: 0,
                worker_generation: 1,
                metadata: None,
                turn_count: None,
                footprint_tokens: None,
                footprint_truth: None,
                title: None,
                agent_metrics: None,
            }],
            next_cursor: None,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: session.clone(),
            attachment: attachment.clone(),
            worker_generation: 1,
            replay_through_seq: 0,
        },
    );
    let raw = |seq, payload| haider_protocol::envelope::EventEnvelope {
        schema_version: 1,
        event_id: haider_protocol::ids::EventId::new(format!("future-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: haider_protocol::ids::DeviceId::new("future-daemon"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: haider_protocol::envelope::RenderTargets {
            ui: true,
            durable: true,
            prompt: haider_protocol::envelope::PromptRender::Omit,
        },
        payload,
    };
    for seq in 1..3 {
        assert!(
            driver
                .apply(
                    &mut model,
                    LiveReply::Event {
                        attachment: attachment.clone(),
                        session: session.clone(),
                        envelope: Box::new(raw(
                            seq,
                            serde_json::json!({"type":"future_event", "seq":seq}),
                        )),
                    },
                )
                .is_empty()
        );
    }
    let report = driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment.clone(),
            session: session.clone(),
            envelope: Box::new(raw(3, serde_json::json!({"type":"future_event"}))),
        },
    );
    assert!(matches!(
        report.as_slice(),
        [LiveCommand::SessionDiagnostic { session: found, code, .. }]
            if found == &session && code == "client-daemon-incompatible"
    ));
    assert_eq!(driver.outbox_len(), 1, "the diagnostic survives reconnect");
    assert_eq!(
        model
            .compatibility_diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.subcode.as_str()),
        Some("client-daemon-incompatible")
    );

    assert!(
        driver
            .apply(
                &mut model,
                LiveReply::Event {
                    attachment,
                    session: session.clone(),
                    envelope: Box::new(raw(4, serde_json::json!({"type":"future_event"}))),
                },
            )
            .is_empty(),
        "the latch emits one durable report, not a storm"
    );
}
