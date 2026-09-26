#![allow(clippy::expect_used)]

use async_trait::async_trait;
use base64::Engine as _;
use haider_core::{
    ArtifactReader, CancelToken, HarnessActor, HarnessConfig, MemoryStore, PromptHistoryCompiler,
    RequestInputCheckpoint, StoreHandle, SubmitCheckpointTurn, SubmitTurn, ToolDispatchResult,
    ToolDispatcher,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, ItemId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolArgumentsFinalizedV1, TurnItem};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::state::RunState;
use haider_protocol::tool::{BoundedResult, ImageBlockRef};
use haider_provider::{FakeProvider, FakeStep, Message, ToolDefinition};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct ApprovalDispatcher {
    approved: AtomicBool,
    menu: Menu,
    seen_args: Mutex<Option<serde_json::Value>>,
    seen_card_args: Mutex<Option<String>>,
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
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: vec![self.image.clone()],
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
            orchestration: None,
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
        args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        *self.seen_args.lock().expect("seen args lock") = Some(args);
        if self.approved.load(Ordering::Acquire) {
            Ok(ToolDispatchResult::Completed(BoundedResult {
                preview: "approved".into(),
                truncated: false,
                truncation: None,
                effects: Vec::new(),
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: haider_protocol::tool::ToolResultStatus::Completed,
                reason: None,
                presentation: None,
                orchestration: None,
            }))
        } else {
            Ok(ToolDispatchResult::ApprovalRequired(self.menu.clone()))
        }
    }

    async fn activate_approval(
        &self,
        _run_id: &RunId,
        checkpoint: &RequestInputCheckpoint,
    ) -> Result<(), HaiderError> {
        *self.seen_card_args.lock().expect("card args lock") =
            Some(checkpoint.display_args.clone());
        Ok(())
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
        .expect("answer serializes")
        .into(),
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
    // Construct secret-shaped content at runtime so no scanner-shaped
    // credential literal ever enters repository history (registry #154).
    let secret = ["sk", "-", "abcdefghijklmnop", "QRSTUV"].concat();
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "write-1".into(),
            name: "fs_write".into(),
            args: serde_json::json!({
                "path":"result.txt",
                "content":"ok",
                "password":"short-secret",
                "token": secret.clone(),
            }),
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
            file_review: None,
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
        seen_args: Mutex::new(None),
        seen_card_args: Mutex::new(None),
    });
    let mut config =
        HarnessConfig::for_session(session_id.clone(), DeviceId::new("permission-device"), 3, 7);
    config.tools = vec![ToolDefinition {
        name: "fs_write".into(),
        description: "write".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let mut live = handle.subscribe();
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
            serde_json::from_value::<EventPayload>(event.payload.clone().into()).is_ok_and(
                |payload| matches!(payload, EventPayload::MenuOpened(ref opened) if opened.id == menu),
            )
        })
        .expect("opening is durable");
    let carrier_envelope = store
        .events(&session_id)
        .await
        .into_iter()
        .find(|event| {
            event.payload.decode_event().is_ok_and(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { ref kind, .. },
                        ..
                    }) if kind == haider_protocol::item::TOOL_ARGUMENTS_FINALIZED_EXTENSION_KIND
                )
            })
        })
        .expect("arguments-finalized carrier is durable before the Ask card");
    assert!(
        carrier_envelope.seq < opening.seq,
        "finalized arguments must precede the approval card"
    );
    let EventPayload::Item(ItemEvent::Completed { item, .. }) = carrier_envelope
        .payload
        .decode_event()
        .expect("typed carrier payload")
    else {
        panic!("carrier envelope shape")
    };
    let carrier = ToolArgumentsFinalizedV1::try_from_extension_item(&item)
        .expect("valid typed carrier")
        .expect("matching carrier kind");
    assert_eq!(carrier.call_id, "write-1");
    assert_eq!(carrier.name, "fs_write");
    assert_eq!(carrier.arguments["path"], "result.txt");
    assert_eq!(carrier.arguments["password"], "[REDACTED:password]");
    assert_eq!(carrier.arguments["token"], "[REDACTED:api_key]");
    assert!(!carrier_envelope.render.ui);
    assert_eq!(carrier_envelope.render.prompt, PromptRender::Omit);
    let mut answer = [committed_answer(opening, menu)];
    store.append(&mut answer).await.expect("commit answer");
    handle
        .apply_committed_menu_event(answer[0].clone())
        .expect("wake permission waiter");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    let raw_args = dispatcher
        .seen_args
        .lock()
        .expect("seen args lock")
        .clone()
        .expect("dispatcher receives arguments");
    assert_eq!(raw_args["password"], "short-secret");
    assert_eq!(raw_args["token"], secret);
    let card_args = dispatcher
        .seen_card_args
        .lock()
        .expect("card args lock")
        .clone()
        .expect("approval checkpoint carries display arguments");
    assert_eq!(
        card_args.as_bytes(),
        serde_json::to_string(&carrier.arguments)
            .expect("carrier arguments serialize")
            .as_bytes(),
        "permission-card display bytes must equal the carrier's redacted argument bytes"
    );
    assert!(!card_args.contains("short-secret"));
    assert!(!card_args.contains(&secret));

    let run_id = carrier_envelope.run_id.clone().expect("carrier run id");
    let journal = store.events(&session_id).await;
    let carrier_positions = journal
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            event
                .payload
                .decode_event()
                .is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::Item(ItemEvent::Started {
                            item: TurnItem::Extension { ref kind, .. },
                            ..
                        } | ItemEvent::Completed {
                            item: TurnItem::Extension { ref kind, .. },
                            ..
                        }) if kind == haider_protocol::item::TOOL_ARGUMENTS_FINALIZED_EXTENSION_KIND
                    )
                })
                .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(carrier_positions.len(), 2, "one carrier item pair");
    assert_eq!(carrier_positions[1], carrier_positions[0] + 1);
    let projection_store = MemoryStore::new();
    let mut before_carrier_journal = journal[..carrier_positions[0]].to_vec();
    StoreHandle::append(&projection_store, &mut before_carrier_journal)
        .await
        .expect("copy journal prefix before carrier");
    let before_messages =
        PromptHistoryCompiler::compile(&projection_store, &session_id, None, None, &run_id)
            .await
            .expect("compile provider request before carrier");
    let mut carrier_pair = journal[carrier_positions[0]..=carrier_positions[1]].to_vec();
    StoreHandle::append(&projection_store, &mut carrier_pair)
        .await
        .expect("append carrier pair");
    let after_messages =
        PromptHistoryCompiler::compile(&projection_store, &session_id, None, None, &run_id)
            .await
            .expect("compile provider request after carrier");
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "approval fixture has one follow-up request"
    );
    assert_eq!(before_messages, requests[0].messages);
    let mut before_carrier = requests[0].clone();
    before_carrier.messages = before_messages;
    let mut after_carrier = before_carrier.clone();
    after_carrier.messages = after_messages;
    assert_eq!(
        serde_json::to_vec(&before_carrier).expect("serialize request without carrier"),
        serde_json::to_vec(&after_carrier).expect("serialize request after carrier"),
        "appending the hidden carrier must not change any provider-request byte"
    );

    let mut live_carrier = None;
    loop {
        match live.try_recv() {
            Ok(event) if event.event_id == carrier_envelope.event_id => {
                live_carrier = Some(event);
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("live carrier replay parity subscriber lagged by {skipped}")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    assert_eq!(
        live_carrier.as_ref(),
        Some(&carrier_envelope),
        "live publication and durable replay must expose identical carrier bytes"
    );
}

#[tokio::test]
async fn tool_script_carrier_card_and_dispatch_preserve_redacted_raw_source() {
    let session_id = SessionId::new("tool-script-carrier-session");
    let store = Arc::new(MemoryStore::new());
    let raw_source = concat!(
        "{\n",
        "  \"version\": 1,\n",
        "  \"duplicate\": \"first\",\n",
        "  \"duplicate\": \"second\",\n",
        "  \"password\": \"short-secret\"\n",
        "}"
    )
    .to_owned();
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCallStart {
            call_id: "script-1".into(),
            name: "tool_script".into(),
        },
        FakeStep::EmitToolArgsDelta {
            call_id: "script-1".into(),
            fragment: raw_source.clone(),
        },
        FakeStep::EmitToolCallEnd {
            call_id: "script-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "script-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let menu = Menu {
        id: MenuId::new("tool-script-permission-menu"),
        kind: MenuKind::Permission {
            effect_summary: "run script child".into(),
            file_review: None,
        },
        title: "tool_script requests approval".into(),
        body: vec!["Allow this exact script?".into()],
        options: vec![MenuOption {
            key: "approve_once".into(),
            label: "Allow once".into(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "union-test-dispatcher".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let dispatcher = Arc::new(ApprovalDispatcher {
        approved: AtomicBool::new(false),
        menu,
        seen_args: Mutex::new(None),
        seen_card_args: Mutex::new(None),
    });
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("tool-script-device"),
        3,
        7,
    );
    config.tools = vec![ToolDefinition {
        name: "tool_script".into(),
        description: "execute a typed orchestration script".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider,
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("exercise the union tool_script path"))
        .await
        .expect("turn starts");
    let mut states = handle.state_receiver();
    states
        .wait_for(|state| matches!(state, Some(RunState::PermissionRequired { .. })))
        .await
        .expect("tool_script parks for approval");

    let journal = store.events(&session_id).await;
    let opening = journal
        .iter()
        .find(|event| {
            event.payload.decode_event().is_ok_and(
                |payload| matches!(payload, EventPayload::MenuOpened(ref menu) if menu.id == MenuId::new("tool-script-permission-menu")),
            )
        })
        .cloned()
        .expect("approval card is durable");
    let carriers = journal
        .iter()
        .filter_map(|event| match event.payload.decode_event().ok()? {
            EventPayload::Item(
                ItemEvent::Started { item, .. } | ItemEvent::Completed { item, .. },
            ) => ToolArgumentsFinalizedV1::from_extension_item(&item)
                .filter(|carrier| carrier.call_id == "script-1"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(carriers.len(), 2, "one carrier item pair is durable");
    assert_eq!(carriers[0], carriers[1]);
    let carrier = &carriers[1];
    assert_eq!(carrier.name, "tool_script");
    let redacted_source = carrier
        .arguments
        .as_str()
        .expect("tool_script carrier arguments remain a raw string");
    assert!(
        redacted_source.contains("  \"duplicate\": \"first\",\n  \"duplicate\": \"second\","),
        "whitespace, order, and duplicate keys stay byte-faithful"
    );
    assert!(!redacted_source.contains("short-secret"));
    assert!(redacted_source.contains("[REDACTED:password]"));
    let card_args = dispatcher
        .seen_card_args
        .lock()
        .expect("card args lock")
        .clone()
        .expect("approval checkpoint carries display arguments");
    assert_eq!(
        card_args,
        serde_json::to_string(&carrier.arguments).expect("carrier arguments serialize"),
        "permission-card bytes are identical to carrier bytes"
    );

    let mut answer = [committed_answer(
        opening,
        MenuId::new("tool-script-permission-menu"),
    )];
    store.append(&mut answer).await.expect("commit answer");
    handle
        .apply_committed_menu_event(answer[0].clone())
        .expect("wake permission waiter");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    assert_eq!(
        dispatcher
            .seen_args
            .lock()
            .expect("seen args lock")
            .as_ref(),
        Some(&serde_json::Value::String(raw_source)),
        "dispatcher receives the exact unredacted source, including duplicate keys"
    );
    handle.stop().await.expect("stop actor");
    actor_task.await.expect("actor joined");
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
            file_review: None,
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
                display_args: "{}".into(),
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
            .expect("opening payload")
            .into(),
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
            .any(|event| serde_json::from_value::<EventPayload>(event.payload.clone().into()).is_ok_and(
                |payload| matches!(payload, EventPayload::ToolResult { result, .. } if result.images == [image.clone()])
            ))
    );
}
