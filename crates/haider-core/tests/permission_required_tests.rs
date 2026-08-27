#![allow(clippy::expect_used)]

use async_trait::async_trait;
use base64::Engine as _;
use haider_core::{
    ArtifactReader, CancelToken, HarnessActor, HarnessConfig, MemoryStore, RequestInputCheckpoint,
    StoreHandle, SubmitCheckpointTurn, SubmitTurn, ToolDispatchResult, ToolDispatcher,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, ItemId, MenuId, RunId, SessionId};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::state::RunState;
use haider_protocol::tool::{BoundedResult, ImageBlockRef};
use haider_provider::{FakeProvider, FakeStep, Message, ToolDefinition};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct ApprovalDispatcher {
    approved: AtomicBool,
    menu: Menu,
}

struct FixedArtifactReader {
    artifact: ArtifactRef,
    bytes: Vec<u8>,
}

#[async_trait]
impl ArtifactReader for FixedArtifactReader {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        if artifact == &self.artifact {
            Ok(self.bytes.clone())
        } else {
            Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "fixture artifact is missing",
                false,
            ))
        }
    }
}

struct RecoveredImageApprovalDispatcher {
    menu: Menu,
    image: ImageBlockRef,
    approved: AtomicBool,
}

#[async_trait]
impl ToolDispatcher for RecoveredImageApprovalDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        if !self.approved.load(Ordering::Acquire) {
            return Ok(ToolDispatchResult::ApprovalRequired(self.menu.clone()));
        }
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "recovered capture".into(),
            truncated: false,
            data: None,
            artifact: None,
            images: vec![self.image.clone()],
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }

    async fn resolve_approval(&self, menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
        if menu.id != self.menu.id || answer.menu != menu.id {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "answer does not match the recovered approval menu",
                false,
            ));
        }
        self.approved.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl ToolDispatcher for ApprovalDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        if self.approved.load(Ordering::Acquire) {
            Ok(ToolDispatchResult::Completed(BoundedResult {
                preview: "approved".into(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: haider_protocol::tool::ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            }))
        } else {
            Ok(ToolDispatchResult::ApprovalRequired(self.menu.clone()))
        }
    }

    async fn resolve_approval(&self, menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
        if menu.id != self.menu.id || answer.menu != menu.id {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "answer does not match the approval menu",
                false,
            ));
        }
        self.approved.store(true, Ordering::Release);
        Ok(())
    }
}

fn committed_answer(opening: RawEnvelope, menu: MenuId) -> RawEnvelope {
    RawEnvelope {
        event_id: EventId::new("committed-permission-answer"),
        seq: 0,
        committed_at_ms: 0,
        causation_id: Some(opening.event_id.clone()),
        payload: serde_json::to_value(EventPayload::MenuAnswered(MenuAnswer {
            menu,
            option_key: Some("approve_once".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        }))
        .expect("answer serializes"),
        ..opening
    }
}

/// MUTATION CHECK: park broker approval in `InputRequired` or accept the raw
/// actor answer. Expected runtime failure: the canonical state assertion or
/// fail-closed raw-answer assertion fails before the committed wake resumes.
#[tokio::test]
async fn permission_menu_parks_in_permission_required_and_needs_committed_answer() {
    let session_id = SessionId::new("permission-required-session");
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "write-1".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path":"result.txt","content":"ok"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "write-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let menu = Menu {
        id: MenuId::new("permission-menu-1"),
        kind: MenuKind::Permission {
            effect_summary: "write result.txt".into(),
        },
        title: "fs_write requests approval".into(),
        body: vec!["Allow this exact write?".into()],
        options: vec![MenuOption {
            key: "approve_once".into(),
            label: "Allow once".into(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "test-dispatcher".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let dispatcher = Arc::new(ApprovalDispatcher {
        approved: AtomicBool::new(false),
        menu,
    });
    let mut config =
        HarnessConfig::for_session(session_id.clone(), DeviceId::new("permission-device"), 3, 7);
    config.tools = vec![ToolDefinition {
        name: "fs_write".into(),
        description: "write".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let (actor, handle) =
        HarnessActor::new_with_dispatcher(config, provider, store.clone(), Some(dispatcher));
    tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("write the result"))
        .await
        .expect("turn starts");
    let mut states = handle.state_receiver();
    let parked = states
        .wait_for(|state| matches!(state, Some(RunState::PermissionRequired { .. })))
        .await
        .expect("actor parks")
        .clone();
    let Some(RunState::PermissionRequired { menu }) = parked else {
        panic!("wait predicate guarantees PermissionRequired");
    };
    let raw_error = handle
        .answer_menu(MenuAnswer {
            menu: menu.clone(),
            option_key: Some("approve_once".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect_err("raw actor answer must fail closed");
    assert_eq!(raw_error.code, ErrorCode::PermissionDenied);

    let opening = store
        .events(&session_id)
        .await
        .into_iter()
        .find(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                |payload| matches!(payload, EventPayload::MenuOpened(ref opened) if opened.id == menu),
            )
        })
        .expect("opening is durable");
    let mut answer = [committed_answer(opening, menu)];
    store.append(&mut answer).await.expect("commit answer");
    handle
        .apply_committed_menu_event(answer[0].clone())
        .expect("wake permission waiter");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
}

#[tokio::test]
async fn recovered_approval_preserves_image_ref_and_resolves_it_for_continuation() {
    let session_id = SessionId::new("recovered-image-approval-session");
    let run_id = RunId::new("recovered-image-approval-run");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgoKC8oLzc3Nzc3N0E8QUNDQ0FBQUFDQ0NISEhVVVVISEhDQ0hIUFBVVVxfXFdXVVdfX2RkZHh4c3OMjJGsrM//xABMAAEBAAAAAAAAAAAAAAAAAAAABwEBAQAAAAAAAAAAAAAAAAAABQcQAQAAAAAAAAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCAAIABADASIAAhEAAxEA/9oADAMBAAIRAxEAPwCOAL+Kf//Z")
        .expect("valid JPEG fixture");
    let artifact = ArtifactRef::new(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let image = ImageBlockRef {
        artifact: artifact.clone(),
        media_type: "image/jpeg".into(),
        width: 16,
        height: 8,
        byte_len: bytes.len() as u64,
    };
    let menu = Menu {
        id: MenuId::new("recovered-image-menu"),
        kind: MenuKind::Permission {
            effect_summary: "capture screen".into(),
        },
        title: "capture requests approval".into(),
        body: vec!["Allow this capture?".into()],
        options: vec![MenuOption {
            key: "approve_once".into(),
            label: "Allow once".into(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "recovery-fixture".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::ExpectToolResult {
                call_id: "capture-after-restart".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let store = Arc::new(MemoryStore::new());
    let dispatcher = Arc::new(RecoveredImageApprovalDispatcher {
        menu: menu.clone(),
        image: image.clone(),
        approved: AtomicBool::new(false),
    });
    let reader = Arc::new(FixedArtifactReader {
        artifact: artifact.clone(),
        bytes: bytes.clone(),
    });
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("recovered-image-device"),
        3,
        7,
    );
    config.tool_result_images_supported = true;
    config.tools = vec![ToolDefinition {
        name: "capture".into(),
        description: "capture".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let (actor, handle) = HarnessActor::new_with_dispatcher_and_artifacts(
        config,
        provider.clone(),
        store.clone(),
        Some(dispatcher),
        Some(reader),
    );
    tokio::spawn(actor.run());
    let turn = handle
        .submit_checkpoint_turn(SubmitCheckpointTurn {
            run_id: run_id.clone(),
            messages: vec![Message::user_text("recover capture")],
            checkpoint: RequestInputCheckpoint {
                menu: menu.clone(),
                request_seq: 4,
                opening_generation: 6,
                tool_item_id: ItemId::new("recovered-image-item"),
                call_id: "capture-after-restart".into(),
                tool_name: "capture".into(),
                args: "{}".into(),
            },
        })
        .await
        .expect("recovered turn starts");
    let opening = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("recovered-image-opening"),
        seq: 4,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("recovered-image-device"),
        authority_epoch: 3,
        worker_generation: 6,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload: serde_json::to_value(EventPayload::MenuOpened(menu.clone()))
            .expect("opening payload"),
    };
    handle
        .apply_committed_menu_event(committed_answer(opening, menu.id))
        .expect("wake recovered approval");

    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        requests[0].messages.last().map(|message| message.blocks.as_slice()),
        Some([Block::ToolResult { images, .. }]) if images == std::slice::from_ref(&image)
    ));
    let resolved = requests[0]
        .attachments
        .iter()
        .find(|resolved| resolved.artifact == artifact)
        .expect("recovered image bytes resolved");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&resolved.data_base64)
            .expect("resolved base64"),
        bytes
    );
    assert!(
        store
            .events(&session_id)
            .await
            .iter()
            .any(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                |payload| matches!(payload, EventPayload::ToolResult { result, .. } if result.images == [image.clone()])
            ))
    );
}
