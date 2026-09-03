//! Workspace-loss projection and recovery-action pins.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_protocol::state::RunState;
use haider_protocol::workspace::{
    WorkspaceEventPayload, WorkspaceUnavailable, WorkspaceUnavailableReason,
};
use haider_rpc::RequestBody;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::link::{command_required_features, request_body};
use haider_tui::live::{LiveCommand, LiveDriver};

mod common;
use common::launcher_model;

fn unavailable(path: &str) -> WorkspaceEventPayload {
    WorkspaceEventPayload::WorkspaceUnavailable(WorkspaceUnavailable {
        path: path.to_owned(),
        reason: WorkspaceUnavailableReason::Missing,
        detail: "No such file or directory".to_owned(),
    })
}

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1.to_owned()]
        .into_iter()
        .collect();
    model.cwd = "/current/project".to_owned();
    let session = SessionId::new("s-wsroot");
    model.sessions.clear();
    model.upsert_live_session(&session);
    model.open_session(&session);
    model
}

#[test]
fn workspace_notice_is_visible_and_not_counted_unknown() {
    let mut model = live_model();
    model
        .projection
        .apply_workspace_event(&unavailable("/gone"));

    assert_eq!(
        model
            .projection
            .workspace_unavailable()
            .map(|notice| notice.path.as_str()),
        Some("/gone")
    );
    assert_eq!(model.projection.unknown_payloads(), 0);
    assert!(model.projection.entries().iter().any(|entry| {
        matches!(
            entry,
            haider_tui::projection::TranscriptEntry::Note { text }
                if text.contains("workspace unavailable") && text.contains("/gone")
        )
    }));

    model
        .projection
        .apply(&haider_protocol::EventPayload::RunState(RunState::Thinking));
    assert_eq!(
        model.projection.workspace_unavailable(),
        None,
        "a new turn drops the prior turn's availability result before its own probe fact"
    );
}

#[test]
fn retry_offers_current_cwd_then_maps_to_workspace_set_wire() {
    let mut model = live_model();
    model
        .projection
        .apply_workspace_event(&unavailable("/gone"));
    model.issue_run_retry();

    assert_eq!(
        model.flash.as_deref(),
        Some("· /retry — re-root to /current/project")
    );
    let request = model.requests.pop().expect("workspace recovery request");
    assert!(matches!(
        &request,
        AppRequest::WorkspaceSet {
            path,
            retry_after: false,
            ..
        } if path == "/current/project"
    ));

    let mut driver = LiveDriver::new("test");
    let commands = driver.handle_request(&mut model, request);
    let command = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::WorkspaceSet { .. }))
        .expect("workspace set command");
    assert_eq!(
        command_required_features(&command),
        &[haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1],
        "durable recovery is gated again at the final send boundary"
    );
    let command_id = match &command {
        LiveCommand::WorkspaceSet { command_id, .. } => command_id.clone(),
        _ => unreachable!("workspace command asserted above"),
    };
    assert!(matches!(
        request_body(command),
        RequestBody::SessionWorkspaceSet { path, .. } if path == "/current/project"
    ));
    driver.apply(
        &mut model,
        haider_tui::live::LiveReply::Failed {
            command_id: Some(command_id),
            code: "feature_missing".into(),
            message: "connected daemon does not advertise session_workspace_set_v1".into(),
            retryable: false,
            presentation: None,
        },
    );
    assert!(
        !model.retry_inflight,
        "feature refusal releases retry latch"
    );
    assert_eq!(
        model.flash.as_deref(),
        Some(
            "· workspace recovery failed — connected daemon does not advertise session_workspace_set_v1"
        )
    );
}
