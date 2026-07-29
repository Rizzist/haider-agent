#![allow(clippy::expect_used)]

use haider_core::{MemoryStore, PromptHistoryCompiler, StoreHandle};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, PROVIDER_OPAQUE_EXTENSION_KIND};
use haider_protocol::state::RunState;

fn envelope(
    session_id: &SessionId,
    run_id: &RunId,
    event_id: &str,
    payload: EventPayload,
    prompt: PromptRender,
) -> haider_protocol::envelope::RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("opaque-history-test"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt,
        },
        payload: serde_json::to_value(payload).expect("payload"),
    }
}

#[tokio::test]
async fn provider_opaque_extension_rehydrates_for_a_terminal_prior_run() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("opaque-history-session");
    let prior = RunId::new("opaque-prior");
    let current = RunId::new("opaque-current");
    let opaque = serde_json::json!({
        "id": "rs_sanitized",
        "type": "reasoning",
        "encrypted_content": "encrypted-synthetic-continuation",
        "summary": []
    });
    let mut events = vec![
        envelope(
            &session_id,
            &prior,
            "opaque-prior-user",
            EventPayload::UserMessage {
                text: "first".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "opaque-prior-item",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("opaque-item"),
                item: TurnItem::Extension {
                    kind: PROVIDER_OPAQUE_EXTENSION_KIND.into(),
                    data: serde_json::json!({
                        "provider": "openai",
                        "data": opaque.clone(),
                    }),
                },
            }),
            PromptRender::Verbatim,
        ),
        envelope(
            &session_id,
            &prior,
            "opaque-prior-done",
            EventPayload::RunState(RunState::Done),
            PromptRender::Omit,
        ),
        envelope(
            &session_id,
            &current,
            "opaque-current-user",
            EventPayload::UserMessage {
                text: "continue".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
            PromptRender::Verbatim,
        ),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("append opaque history");

    let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
        .await
        .expect("compile opaque history");

    assert!(messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ProviderOpaque { provider, data }
                    if provider == "openai" && data == &opaque
            )
        })
    }));
}
