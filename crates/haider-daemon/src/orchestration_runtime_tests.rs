#![allow(clippy::expect_used)]

use super::mobile_runtime_tests::{MobileDispatcherFixture, mobile_dispatcher_fixture_with_policy};
use super::*;
use haider_core::{CancelToken, StoreHandle, ToolDispatchResult};
use haider_protocol::effect::EffectPhase;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::orchestration::ScriptTerminalStatusV1;
use haider_protocol::tool::ToolResultStatus;
use haider_tools::FakeMobileBackend;
use std::sync::Arc;

fn fs_read_script(path: &str) -> String {
    let entry = registered_tool_by_name("fs_read").expect("fs_read registry entry");
    let wrapper_digest = orchestration_wrapper_digest(entry);
    let factory = BrokerToolFactory::with_mobile_backend(Arc::new(FakeMobileBackend::default()));
    let child_tools = orchestration_entry_child_tools(&factory, None, None, &[], false);
    let catalog_digest =
        orchestration_catalog_digest_for_names(registered_tools(), Some(&child_tools));
    serde_json::json!({
        "version": 1,
        "transport": haider_protocol::orchestration::ORCHESTRATION_TRANSPORT,
        "catalog_digest": catalog_digest,
        "graph": {
            "kind": "inline",
            "parameters": [],
            "nodes": [
                {
                    "slot": 0,
                    "evidence_type": "OrchValueV1",
                    "config": {
                        "operator": "literal",
                        "type": {"kind": "record", "fields": {
                            "path": {"kind": "string", "max_bytes": 4096}
                        }},
                        "operand_config": {"value": {"path": path}},
                        "region": []
                    },
                    "ports": []
                },
                {
                    "slot": 1,
                    "evidence_type": "OrchArgumentV1",
                    "config": {"tool": "fs_read", "wrapper_digest": wrapper_digest, "region": []},
                    "ports": [
                        {"role": "data", "port": "args", "source_slot": 0, "output": "value"}
                    ]
                },
                {
                    "slot": 2,
                    "evidence_type": "OrchRetryPolicyV1",
                    "config": {"max_attempts": 1},
                    "ports": []
                },
                {
                    "slot": 3,
                    "evidence_type": "OrchAskPauseV1",
                    "config": {
                        "tool": "fs_read",
                        "wrapper_digest": wrapper_digest,
                        "owner_call_slot": 4,
                        "wait_ms": 120000,
                        "region": []
                    },
                    "ports": [
                        {"role": "data", "port": "args", "source_slot": 1, "output": "value"}
                    ]
                },
                {
                    "slot": 4,
                    "evidence_type": "OrchCallV1",
                    "config": {"tool": "fs_read", "wrapper_digest": wrapper_digest, "region": []},
                    "ports": [
                        {"role": "data", "port": "args", "source_slot": 1, "output": "value"},
                        {"role": "control", "port": "permit", "source_slot": 3, "output": "permit"},
                        {"role": "config", "port": "retry", "source_slot": 2, "output": "policy"}
                    ]
                },
                {
                    "slot": 5,
                    "evidence_type": "OrchAwaitV1",
                    "config": {"on_error": "stop", "region": []},
                    "ports": [
                        {"role": "data", "port": "operation", "source_slot": 4, "output": "operation"}
                    ]
                },
                {
                    "slot": 6,
                    "evidence_type": "OrchExitV1",
                    "config": {"mode": "return", "region": []},
                    "ports": [
                        {"role": "data", "port": "value", "source_slot": 5, "output": "value"}
                    ]
                }
            ],
            "exits": [6],
            "read_groups": []
        },
        "inputs": []
    })
    .to_string()
}

async fn orchestration_journal_counts(
    fixture: &MobileDispatcherFixture,
) -> (usize, usize, usize, usize, usize) {
    let mut cursor = 0;
    let mut intents = 0;
    let mut authorized = 0;
    let mut dispatched = 0;
    let mut outcomes = 0;
    let mut child_results = 0;
    loop {
        let page = StoreHandle::read_reducer_page_with_boundary(
            &fixture.store,
            &fixture.session_id,
            cursor,
            1_024,
            4 * 1_024 * 1_024,
            &["effect", "tool_result", "item"],
        )
        .await
        .expect("read orchestration journal")
        .envelopes;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            cursor = envelope.seq;
            match envelope.payload.decode_event() {
                Ok(EventPayload::Effect(EffectPhase::Intent(_))) => intents += 1,
                Ok(EventPayload::Effect(EffectPhase::Authorized { .. })) => authorized += 1,
                Ok(EventPayload::Effect(EffectPhase::Dispatched { .. })) => dispatched += 1,
                Ok(EventPayload::Effect(EffectPhase::Outcome { .. })) => outcomes += 1,
                Ok(EventPayload::ToolResult { call_id, .. }) if call_id.starts_with("orch:") => {
                    child_results += 1;
                }
                Ok(EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { .. },
                    ..
                })) => {}
                _ => {}
            }
        }
    }
    (intents, authorized, dispatched, outcomes, child_results)
}

#[tokio::test]
async fn tool_script_brokers_each_child_and_replays_committed_terminal_without_effects() {
    let fixture = mobile_dispatcher_fixture_with_policy(
        "orchestration-fs-read",
        "exercise orchestration",
        Arc::new(FakeMobileBackend::default()),
        None,
        true,
    )
    .await;
    let path = std::path::Path::new(&fixture.cwd).join("input.txt");
    std::fs::write(&path, "brokered orchestration\n").expect("write fixture");
    let script = fs_read_script(&path.to_string_lossy());
    let item_id = ItemId::new("outer-script-item");
    let first = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &item_id,
            "outer-script-call",
            "tool_script",
            serde_json::Value::String(script.clone()),
            &CancelToken::new(),
        )
        .await
        .expect("execute script");
    let ToolDispatchResult::Completed(first) = first else {
        panic!("script unexpectedly requested approval");
    };
    assert_eq!(
        first.status,
        ToolResultStatus::Completed,
        "{}",
        first.preview
    );
    let terminal = first.orchestration.expect("structured terminal");
    assert_eq!(terminal.status, ScriptTerminalStatusV1::Completed);
    assert_eq!(terminal.counts.calls, 1);
    assert_eq!(terminal.counts.attempts, 1);
    assert!(terminal.terminal_ref.is_some());

    let before = orchestration_journal_counts(&fixture).await;
    assert_eq!(before, (1, 1, 1, 1, 1));

    let replay = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &item_id,
            "outer-script-call",
            "tool_script",
            serde_json::Value::String(script),
            &CancelToken::new(),
        )
        .await
        .expect("replay committed script");
    let ToolDispatchResult::Completed(replay) = replay else {
        panic!("terminal replay unexpectedly requested approval");
    };
    assert_eq!(replay.status, ToolResultStatus::Completed);
    assert_eq!(orchestration_journal_counts(&fixture).await, before);
}
