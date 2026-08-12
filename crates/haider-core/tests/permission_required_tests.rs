#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CancelToken, HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn,
    ToolDispatchResult, ToolDispatcher,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EventId, ItemId, MenuId, RunId, SessionId};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_provider::{FakeProvider, FakeStep, ToolDefinition};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct ApprovalDispatcher {
    approved: AtomicBool,
    menu: Menu,
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
                artifact: None,
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
