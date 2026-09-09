#![allow(clippy::expect_used)]

mod common;

use haider_protocol::ids::SessionId;
use haider_rpc::ERROR_CODE_BUSY;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;

/// E7 visual law: the demo-mode live-only notice is a MATTER-OF-FACT status
/// line — the canonical `/peer` registry message in one quiet flash, gone on
/// the next keystroke. It never latches the persistent diagnostic banner.
#[test]
fn e7_peers_alias_uses_the_quiet_peer_registry_notice() {
    let mut model = common::launcher_model();
    common::submit(&mut model, "/peers");

    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text == "· /peer — live only; the registry is daemon truth"),
        "the canonical live-only message rides the flash line"
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
