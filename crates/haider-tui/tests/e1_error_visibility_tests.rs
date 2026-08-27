#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tui::plain::render_plain;
use haider_tui::projection::SessionProjection;

fn apply_tool(
    projection: &mut SessionProjection,
    id: &str,
    call_id: &str,
    name: &str,
    status: ToolResultStatus,
    reason: Option<&str>,
) {
    let item_id = ItemId::new(id);
    let started = TurnItem::ToolCall {
        call_id: call_id.into(),
        name: name.into(),
        args: serde_json::json!({}),
        status: ToolStatus::InProgress,
    };
    projection.apply(&EventPayload::Item(ItemEvent::Started {
        item_id: item_id.clone(),
        item: started,
    }));
    projection.apply(&EventPayload::ToolResult {
        call_id: call_id.into(),
        result: BoundedResult {
            preview: "{}".into(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status,
            reason: reason.map(str::to_owned),
            presentation: None,
        },
    });
    projection.apply(&EventPayload::Item(ItemEvent::Completed {
        item_id,
        item: TurnItem::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            args: serde_json::json!({}),
            status: status.item_status(),
        },
    }));
}

/// LAW E1a: denied, nonzero process, and edit-anchor failure rows are visibly
/// non-green and carry a bounded reason; successful tools remain green.
/// MUTATION: force `BoundedResult.status` to Completed at the actor join and
/// the failed-row glyph assertions fail at runtime.
#[test]
fn e1a_tool_terminal_status_and_reason_render_inline() {
    let mut projection = SessionProjection::default();
    apply_tool(
        &mut projection,
        "i-denied",
        "c-denied",
        "fs_write",
        ToolResultStatus::Rejected,
        Some("effect denied by policy"),
    );
    apply_tool(
        &mut projection,
        "i-exit",
        "c-exit",
        "process_exec",
        ToolResultStatus::Failed,
        Some("process exited with code 1"),
    );
    apply_tool(
        &mut projection,
        "i-anchor",
        "c-anchor",
        "fs_edit",
        ToolResultStatus::Conflict,
        Some("edit anchor matched 0 occurrences"),
    );
    apply_tool(
        &mut projection,
        "i-ok",
        "c-ok",
        "process_exec",
        ToolResultStatus::Completed,
        None,
    );

    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("⚒ fs_write ✗ · effect denied by policy"));
    assert!(rendered.contains("⚒ process_exec ✗ · process exited with code 1"));
    assert!(rendered.contains("⚒ fs_edit ✗ · edit anchor matched 0 occurrences"));
    assert!(rendered.contains("⚒ process_exec ✓"));
}

#[test]
fn e1b_refusal_item_is_visible_in_done_transcript() {
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("refusal"),
        item: TurnItem::Refusal {
            reason: "The model declined this request.".into(),
        },
    }));
    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("✗ model refused — The model declined this request."));
}

#[test]
fn e2a_typed_run_failure_render_never_uses_legacy_body_marker() {
    const MARKER: &str = "RAW_BODY_MUST_NEVER_RENDER_98c4";
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::RunFailed {
        code: ErrorCode::ProviderError,
        message: MARKER.into(),
        retryable: true,
        presentation: Some(ErrorPresentation::new(
            "rate-limited",
            "Provider rate limit reached",
            "Wait for the provider limit to reset, then retry.",
            ErrorScope::Account,
            [ErrorAction::Wait, ErrorAction::Retry],
        )),
    });
    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("Provider rate limit reached"));
    assert!(rendered.contains("[rate-limited]"));
    assert!(rendered.contains("actions: wait, retry"));
    assert!(!rendered.contains(MARKER));
}

#[test]
fn e4_incomplete_assistant_item_has_explicit_plain_label() {
    let mut projection = SessionProjection::default();
    projection.apply(&EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("partial"),
        item: TurnItem::IncompleteAgentMessage {
            text: "partial answer".into(),
            interruption: ErrorPresentation::new(
                "stream-interrupted",
                "Response stream interrupted",
                "The provider connection ended after content.",
                ErrorScope::Turn,
                [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
            ),
        },
    }));
    let rendered = render_plain(&projection, 0, None);
    assert!(rendered.contains("partial answer"));
    assert!(rendered.contains("⚠ incomplete — stream interrupted (stream-interrupted)"));
}
