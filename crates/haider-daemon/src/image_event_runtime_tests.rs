//! Durable image-created output-context regression pins.

#![allow(clippy::expect_used)]

use super::HubCommandOutputContext;
use crate::session_hub::{SessionHub, SessionHubConfig};
use haider_core::{
    EventIdGenerator, SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
};
use haider_protocol::envelope::PromptRender;
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::image::{IMAGE_CREATED_EXTENSION_KIND, ImageCreatedV1};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::{DeliveryMode, EventPayload};
use std::sync::Arc;

/// MUTATION CHECK: omitting the append, changing the extension kind or
/// payload, or allowing prompt rendering makes the durable scan fail.
#[tokio::test]
async fn image_output_context_commits_self_contained_omitted_prompt_extension() {
    let root = tempfile::tempdir().expect("profile");
    let sqlite = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(sqlite, SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("image-created-output-context");
    let device_id = DeviceId::new("image-created-device");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-image-session".into(),
        request_digest: "create-image-session-digest".into(),
        request_json: r#"{"session":"image-created"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .display()
            .to_string(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: super::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("image-session-created"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create session");
    let run_id = RunId::new("image-run");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "submit-image-run".into(),
        request_digest: "submit-image-run-digest".into(),
        request_json: r#"{"turn":"image"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: "create image".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new("image-run-queued"),
        user_event_id: EventId::new("image-run-user"),
        active_event_id: EventId::new("image-run-active"),
        device_id: device_id.clone(),
    })
    .await
    .expect("accept image run");
    let lease = hub
        .acquire_worker_lease(session_id)
        .await
        .expect("worker lease");
    let output = HubCommandOutputContext {
        store: lease.clone(),
        branch_id: None,
        agent_id: None,
        device_id,
        event_ids: Arc::new(EventIdGenerator::new("image-event")),
    };
    let expected = ImageCreatedV1 {
        path: "/workspace/chart.png".into(),
        display_path: "chart.png".into(),
        media_type: "image/png".into(),
        byte_len: 24,
        width: Some(640),
        height: Some(480),
        call_id: "call-image".into(),
        tool: "process_exec".into(),
    };

    output
        .append_image_created(&run_id, expected.clone())
        .await
        .expect("append image event");

    let events = StoreHandle::read(&lease, lease.session_id(), 0, 64)
        .await
        .expect("read events");
    let event = events
        .iter()
        .find(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { ref kind, .. },
                        ..
                    }) if kind == IMAGE_CREATED_EXTENSION_KIND
                )
            })
        })
        .expect("durable image extension");
    assert_eq!(event.render.prompt, PromptRender::Omit);
    let EventPayload::Item(ItemEvent::Completed {
        item: TurnItem::Extension { data, .. },
        ..
    }) = serde_json::from_value(event.payload.clone()).expect("decode event")
    else {
        panic!("expected completed image extension");
    };
    assert_eq!(
        serde_json::from_value::<ImageCreatedV1>(data).expect("decode payload"),
        expected
    );
}
