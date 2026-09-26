#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::mobile_transport::Envelope;
use haider_core::{MemoryStore, StoreHandle};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};

#[test]
fn mobile_chat_creation_derives_the_selected_models_output_budget() {
    let request = mobile_session_create_request(
        "mobile-test".into(),
        "/tmp".into(),
        "gemini".into(),
        "gemini-2.0-flash".into(),
    );
    assert!(
        matches!(request, RequestBody::SessionCreateWithPermissionOverrides {
        max_tokens: 0, provider, model, ..
    } if provider == "gemini" && model == "gemini-2.0-flash")
    );
}

#[tokio::test]
async fn mobile_transport_text_durably_activates_mobile_use() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("mobile-bridge-activation");
    let text = activate_mobile_use("look at my latest SMS");
    assert!(crate::worker::explicit_mobile_use_intent(&text));
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("mobile-bridge-user"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("mobile-bridge-run")),
        agent_id: None,
        device_id: DeviceId::new("mobile-bridge-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(EventPayload::UserMessage {
            text,
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        })
        .expect("mobile activation payload")
        .into(),
    };
    store
        .append(&mut [envelope])
        .await
        .expect("append mobile activation");
    assert!(
        crate::worker::durable_session_tool_state(&store, &session_id)
            .await
            .expect("durable mobile state")
            .mobile_use_active
    );
}

#[test]
fn explicit_mobile_marker_is_not_duplicated() {
    let text = "/mobile-use read messages";
    assert_eq!(activate_mobile_use(text), text);
}

#[tokio::test]
async fn hub_event_admission_reserves_a_response_slot() {
    let (frames, mut receiver) = mpsc::channel(4);
    let sink = MobileHubSink {
        frames,
        waiters: StdMutex::new(VecDeque::new()),
    };
    let attachment = haider_rpc::AttachmentId::new("mobile-pressure");
    for nonce in 0..3 {
        assert_eq!(
            crate::FrameSink::offer(&sink, &attachment, &WireFrame::Ping { nonce }),
            crate::SendAdmission::Sent
        );
    }
    assert_eq!(
        crate::FrameSink::offer(&sink, &attachment, &WireFrame::Ping { nonce: 4 }),
        crate::SendAdmission::Busy
    );
    crate::FrameSink::try_send(&sink, WireFrame::Pong { nonce: 5 })
        .expect("reserved response slot");
    assert_eq!(receiver.recv().await, Some(WireFrame::Ping { nonce: 0 }));
}

#[test]
fn pending_response_registration_cleans_up_when_cancelled() {
    let responses = Arc::new(StdMutex::new(HashMap::new()));
    let request_id = RequestId::new("mobile-cancelled-response");
    let (reply, _receiver) = oneshot::channel();
    let pending = PendingHubResponse::register(request_id.clone(), reply, Arc::clone(&responses));
    assert!(response_map(&responses).contains_key(&request_id));
    drop(pending);
    assert!(!response_map(&responses).contains_key(&request_id));
}

#[tokio::test]
async fn attachment_lag_is_sticky_without_an_active_turn_subscriber() {
    let (frames, receiver) = mpsc::channel(4);
    let sink = Arc::new(MobileHubSink {
        frames: frames.clone(),
        waiters: StdMutex::new(VecDeque::new()),
    });
    let responses = Arc::new(StdMutex::new(HashMap::new()));
    let (events, _) = broadcast::channel(4);
    let lag_epoch = Arc::new(AtomicU64::new(0));
    let pump = tokio::spawn(pump_hub_frames(
        receiver,
        sink,
        responses,
        events,
        Arc::clone(&lag_epoch),
    ));
    frames
        .send(WireFrame::Lagged {
            attachment_id: haider_rpc::AttachmentId::new("mobile-idle-lag"),
            last_queued_seq: 8,
        })
        .await
        .expect("enqueue lag frame");
    for _ in 0..8 {
        if lag_epoch.load(Ordering::Acquire) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(lag_epoch.load(Ordering::Acquire), 1);
    pump.abort();
    let _ = pump.await;
}

#[tokio::test]
async fn real_turn_events_project_to_thinking_and_answer_deltas() {
    let (frames, mut receiver) = mpsc::channel(8);
    let responder = ChatResponder { id: 91, frames };
    let mut projection = TurnProjection::default();
    let run_id = RunId::new("mobile-projection-run");

    assert!(
        !projection
            .apply(
                raw_event(
                    1,
                    &run_id,
                    EventPayload::Item(ItemEvent::Delta {
                        item_id: ItemId::new("mobile-reasoning"),
                        delta: ItemDelta::Reasoning {
                            text: "checking".into(),
                        },
                    }),
                ),
                &responder,
            )
            .await
            .expect("project reasoning")
    );
    assert!(
        !projection
            .apply(
                raw_event(
                    2,
                    &run_id,
                    EventPayload::Item(ItemEvent::Delta {
                        item_id: ItemId::new("mobile-answer"),
                        delta: ItemDelta::Text {
                            text: "done".into(),
                        },
                    }),
                ),
                &responder,
            )
            .await
            .expect("project answer")
    );
    assert!(
        projection
            .apply(
                raw_event(3, &run_id, EventPayload::RunState(RunState::Done)),
                &responder,
            )
            .await
            .expect("project terminal")
    );
    assert_eq!(
        receiver.recv().await.expect("thinking delta"),
        Envelope {
            id: 91,
            body: serde_json::json!({
                "type": "chat.delta",
                "text": "checking",
                "segment": "thinking",
            }),
        }
    );
    assert_eq!(
        receiver.recv().await.expect("answer delta"),
        Envelope {
            id: 91,
            body: serde_json::json!({
                "type": "chat.delta",
                "text": "done",
                "segment": "answer",
            }),
        }
    );
    assert!(projection.terminal_result().is_ok());
}

#[tokio::test]
async fn run_failure_bridge_uses_safe_presentation_instead_of_raw_message() {
    use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};

    let (frames, _receiver) = mpsc::channel(8);
    let responder = ChatResponder { id: 92, frames };
    let mut projection = TurnProjection::default();
    let run_id = RunId::new("mobile-projection-failure");
    let presentation = ErrorPresentation::new(
        "permission-denied",
        "Provider denied the request",
        "The active account is not allowed to make this request. · details withheld",
        ErrorScope::Turn,
        [ErrorAction::SwitchAccount],
    )
    .with_provider_error_type(Some("permission_error"))
    .with_request_id(Some("req_fixture-403"))
    .with_http_status(403);
    projection
        .apply(
            raw_event(
                1,
                &run_id,
                EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "raw private body".into(),
                    retryable: false,
                    presentation: Some(presentation),
                },
            ),
            &responder,
        )
        .await
        .expect("project failure");
    projection
        .apply(
            raw_event(2, &run_id, EventPayload::RunState(RunState::Errored)),
            &responder,
        )
        .await
        .expect("project terminal");
    let error = projection.terminal_result().expect_err("run failed");
    assert_eq!(error.code, "provider_error");
    assert!(error.message.contains("permission_error"));
    assert!(error.message.contains("HTTP 403"));
    assert!(error.message.contains("req_fixture-403"));
    assert!(!error.message.contains("raw private body"));
}

fn raw_event(seq: u64, run_id: &RunId, payload: EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("mobile-projection-{seq}")),
        seq,
        session_id: SessionId::new("mobile-projection-session"),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("mobile-projection-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(payload)
            .expect("projection payload")
            .into(),
    }
}
