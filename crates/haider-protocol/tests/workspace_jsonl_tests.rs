#![allow(clippy::expect_used)]

use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::workspace::{
    WorkspaceEventPayload, WorkspaceUnavailable, WorkspaceUnavailableReason,
};

/// The CLI's JSONL writer serializes each raw envelope without reduction.
/// Pin the exact additive notice line so path/reason/render coordinates cannot
/// silently drift while all valid-root JSONL goldens remain unchanged.
#[test]
fn workspace_unavailable_notice_jsonl_golden() {
    let payload = WorkspaceEventPayload::WorkspaceUnavailable(WorkspaceUnavailable {
        path: "/private/tmp/vanished/scratchpad".into(),
        reason: WorkspaceUnavailableReason::Missing,
        detail: "No such file or directory (os error 2)".into(),
    })
    .to_payload_value()
    .expect("payload");
    let envelope = RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("event-workspace-unavailable"),
        seq: 7,
        session_id: SessionId::new("session-1"),
        branch_id: None,
        run_id: Some(RunId::new("run-2")),
        agent_id: None,
        device_id: DeviceId::new("device-1"),
        authority_epoch: 3,
        worker_generation: 4,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 5,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    };
    let actual = format!("{}\n", serde_json::to_string(&envelope).expect("JSONL"));
    assert_eq!(
        actual,
        include_str!("fixtures/workspace_unavailable_notice.jsonl")
    );
}
