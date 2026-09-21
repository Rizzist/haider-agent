#![allow(clippy::expect_used)]

use super::mobile_runtime_tests::{
    MobileDispatcherFixture, close_fixture, mobile_dispatcher_fixture_with_policy,
};
use super::*;
use haider_core::{CancelToken, EventIdGenerator, StoreHandle, ToolDispatchResult};
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::ids::{DeviceId, EffectId, ItemId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::orchestration::ScriptTerminalStatusV1;
use haider_protocol::tool::ToolResultStatus;
use haider_tools::FakeMobileBackend;
use std::sync::Arc;

#[derive(Clone, Copy)]
enum CrashBoundary {
    Started,
    Intent,
    Authorized,
    Dispatched,
    Outcome,
    ToolResult,
}

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

fn fs_read_group_script(first_path: &str, second_path: &str) -> String {
    let entry = registered_tool_by_name("fs_read").expect("fs_read registry entry");
    let wrapper_digest = orchestration_wrapper_digest(entry);
    let factory = BrokerToolFactory::with_mobile_backend(Arc::new(FakeMobileBackend::default()));
    let child_tools = orchestration_entry_child_tools(&factory, None, None, &[], false);
    let catalog_digest =
        orchestration_catalog_digest_for_names(registered_tools(), Some(&child_tools));
    let value_type = serde_json::json!({
        "kind": "record",
        "fields": {"path": {"kind": "string", "max_bytes": 4096}}
    });
    let tool_config = |region: serde_json::Value| {
        serde_json::json!({
            "tool": "fs_read",
            "wrapper_digest": wrapper_digest,
            "region": region,
        })
    };
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
                        "operator": "literal", "type": value_type,
                        "operand_config": {"value": {"path": first_path}}, "region": []
                    },
                    "ports": []
                },
                {
                    "slot": 1, "evidence_type": "OrchArgumentV1",
                    "config": tool_config(serde_json::json!([])),
                    "ports": [{"role":"data","port":"args","source_slot":0,"output":"value"}]
                },
                {"slot":2,"evidence_type":"OrchRetryPolicyV1","config":{"max_attempts":1},"ports":[]},
                {
                    "slot":3,"evidence_type":"OrchAskPauseV1",
                    "config":{
                        "tool":"fs_read","wrapper_digest":wrapper_digest,
                        "owner_call_slot":8,"wait_ms":120000,"region":[]
                    },
                    "ports":[{"role":"data","port":"args","source_slot":1,"output":"value"}]
                },
                {
                    "slot":4,
                    "evidence_type":"OrchValueV1",
                    "config":{
                        "operator":"literal","type":value_type,
                        "operand_config":{"value":{"path":second_path}},"region":[]
                    },
                    "ports":[]
                },
                {
                    "slot":5,"evidence_type":"OrchArgumentV1",
                    "config":tool_config(serde_json::json!([])),
                    "ports":[{"role":"data","port":"args","source_slot":4,"output":"value"}]
                },
                {"slot":6,"evidence_type":"OrchRetryPolicyV1","config":{"max_attempts":1},"ports":[]},
                {
                    "slot":7,"evidence_type":"OrchAskPauseV1",
                    "config":{
                        "tool":"fs_read","wrapper_digest":wrapper_digest,
                        "owner_call_slot":9,"wait_ms":120000,"region":[]
                    },
                    "ports":[{"role":"data","port":"args","source_slot":5,"output":"value"}]
                },
                {
                    "slot":8,"evidence_type":"OrchCallV1",
                    "config":tool_config(serde_json::json!([])),
                    "ports":[
                        {"role":"data","port":"args","source_slot":1,"output":"value"},
                        {"role":"control","port":"permit","source_slot":3,"output":"permit"},
                        {"role":"config","port":"retry","source_slot":2,"output":"policy"}
                    ]
                },
                {
                    "slot":9,"evidence_type":"OrchCallV1",
                    "config":tool_config(serde_json::json!([])),
                    "ports":[
                        {"role":"data","port":"args","source_slot":5,"output":"value"},
                        {"role":"control","port":"permit","source_slot":7,"output":"permit"},
                        {"role":"config","port":"retry","source_slot":6,"output":"policy"}
                    ]
                },
                {
                    "slot":10,"evidence_type":"OrchAwaitV1",
                    "config":{"on_error":"stop","region":[]},
                    "ports":[{"role":"data","port":"operation","source_slot":8,"output":"operation"}]
                },
                {
                    "slot":11,"evidence_type":"OrchAwaitV1",
                    "config":{"on_error":"stop","region":[]},
                    "ports":[{"role":"data","port":"operation","source_slot":9,"output":"operation"}]
                },
                {
                    "slot":12,"evidence_type":"OrchJoinV1",
                    "config":{
                        "mode":"all",
                        "type":{
                            "kind":"list",
                            "item":{
                                "kind":"opaque","ref_kind":"tool_result",
                                "issuer_version":"1","decoder_version":"1"
                            },
                            "max_items":2
                        },
                        "omit_inactive":false,"region":[]
                    },
                    "ports":[
                        {"role":"data","port":"first","source_slot":10,"output":"value"},
                        {"role":"data","port":"second","source_slot":11,"output":"value"}
                    ]
                },
                {
                    "slot":13,"evidence_type":"OrchExitV1",
                    "config":{"mode":"return","region":[]},
                    "ports":[{"role":"data","port":"value","source_slot":12,"output":"value"}]
                }
            ],
            "exits":[13],
            "read_groups":[{"id":"reads","members":[8,9],"max_concurrency":2}]
        },
        "inputs":[]
    })
    .to_string()
}

fn hidden_mobile_script() -> String {
    let mut request: serde_json::Value =
        serde_json::from_str(&fs_read_script("unused")).expect("base script JSON");
    let mobile = registered_tool_by_name("mobile").expect("mobile registry entry");
    let wrapper_digest = orchestration_wrapper_digest(mobile);
    let nodes = request["graph"]["nodes"]
        .as_array_mut()
        .expect("inline nodes");
    for slot in [1usize, 3, 4] {
        nodes[slot]["config"]["tool"] = serde_json::Value::String("mobile".into());
        nodes[slot]["config"]["wrapper_digest"] = serde_json::Value::String(wrapper_digest.clone());
    }
    request.to_string()
}

fn retrying_fs_read_script(path: &str) -> String {
    let mut request: serde_json::Value =
        serde_json::from_str(&fs_read_script(path)).expect("base script JSON");
    request["graph"]["nodes"][2]["config"]["max_attempts"] = serde_json::Value::from(3);
    request.to_string()
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

async fn orchestration_activation_clocks(
    fixture: &MobileDispatcherFixture,
) -> Vec<(u64, u64, u64)> {
    let mut cursor = 0;
    let mut clocks = Vec::new();
    loop {
        let page = StoreHandle::read_reducer_page_with_boundary(
            &fixture.store,
            &fixture.session_id,
            cursor,
            1_024,
            4 * 1_024 * 1_024,
            &["item"],
        )
        .await
        .expect("read activation journal")
        .envelopes;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            cursor = envelope.seq;
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                continue;
            };
            if kind != haider_protocol::orchestration::ORCHESTRATION_CALL_EXTENSION
                || data.get("phase").and_then(serde_json::Value::as_str) != Some("started")
            {
                continue;
            }
            let activation: haider_protocol::pipe::InstructEvidenceRef = serde_json::from_value(
                data.get("activation_ref")
                    .cloned()
                    .expect("activation ref field"),
            )
            .expect("activation ref");
            let bytes = fixture
                .worker_store
                .get_artifact_bounded(activation.artifact, activation.byte_len)
                .await
                .expect("read activation evidence");
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("activation payload");
            clocks.push((
                value["activated_at_ms"].as_u64().expect("activation time"),
                value["operation_started_at_ms"]
                    .as_u64()
                    .expect("operation start"),
                value["deadline_ms"].as_u64().expect("deadline"),
            ));
        }
    }
    clocks
}

async fn assert_recovery_boundary(label: &str, boundary: CrashBoundary, effectless_actor: bool) {
    let fixture = mobile_dispatcher_fixture_with_policy(
        label,
        "exercise orchestration recovery",
        Arc::new(FakeMobileBackend::default()),
        None,
        true,
    )
    .await;
    let item_id = ItemId::new(format!("{label}-item"));
    let call_id = format!("{label}-call");
    let effect = EffectId::new(format!("{label}-effect"));
    let mut payloads = vec![EventPayload::Item(ItemEvent::Started {
        item_id: item_id.clone(),
        item: TurnItem::ToolCall {
            call_id: call_id.clone(),
            name: "fs_read".into(),
            args: serde_json::json!({"path":"input.txt"}),
            status: haider_protocol::item::ToolStatus::InProgress,
        },
    })];
    if !matches!(boundary, CrashBoundary::Started) {
        payloads.push(EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect.clone(),
            class: EffectClass::FsRead,
            summary: "synthetic orchestration recovery read".into(),
            args_digest: format!("blake3:{}", "1".repeat(64)),
            workspace_revision: None,
        })));
    }
    if matches!(
        boundary,
        CrashBoundary::Authorized
            | CrashBoundary::Dispatched
            | CrashBoundary::Outcome
            | CrashBoundary::ToolResult
    ) {
        payloads.push(EventPayload::Effect(EffectPhase::Authorized {
            effect: effect.clone(),
            verdict: AuthorizationVerdict::Allow,
        }));
    }
    if matches!(
        boundary,
        CrashBoundary::Dispatched | CrashBoundary::Outcome | CrashBoundary::ToolResult
    ) {
        payloads.push(EventPayload::Effect(EffectPhase::Dispatched {
            effect: effect.clone(),
        }));
    }
    if matches!(boundary, CrashBoundary::Outcome | CrashBoundary::ToolResult) {
        payloads.push(EventPayload::Effect(EffectPhase::Outcome {
            effect,
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: None,
        }));
    }
    if matches!(boundary, CrashBoundary::ToolResult) {
        payloads.push(EventPayload::ToolResult {
            call_id: call_id.clone(),
            result: BoundedResult {
                preview: "recovered".into(),
                truncated: false,
                truncation: None,
                effects: Vec::new(),
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
                orchestration: None,
            },
        });
    }
    append_payloads(
        &fixture.worker_store,
        &DeviceId::new(format!("{label}-device")),
        &fixture.run_id,
        None,
        &EventIdGenerator::new(format!("{label}-event")),
        payloads,
    )
    .await
    .expect("append synthetic crash boundary");

    let recovered = recover_orchestration_child(
        &fixture.worker_store,
        &fixture.run_id,
        &item_id,
        &call_id,
        effectless_actor,
    )
    .await
    .expect("classify orchestration recovery boundary");
    let recovered_debug = format!("{recovered:?}");
    match (boundary, effectless_actor, recovered) {
        (CrashBoundary::ToolResult, _, RecoveredOrchestrationChild::Completed(result)) => {
            assert_eq!(result.preview, "recovered");
        }
        (
            CrashBoundary::Started | CrashBoundary::Intent | CrashBoundary::Authorized,
            false,
            RecoveredOrchestrationChild::SafeToRetry,
        ) => {}
        (
            CrashBoundary::Dispatched,
            false,
            RecoveredOrchestrationChild::Interrupted { unknown: true },
        ) => {}
        (
            CrashBoundary::Outcome,
            false,
            RecoveredOrchestrationChild::Interrupted { unknown: false },
        ) => {}
        (
            CrashBoundary::Started,
            true,
            RecoveredOrchestrationChild::Interrupted { unknown: false },
        ) => {}
        _ => panic!("unexpected recovery classification at {label}: {recovered_debug}"),
    }
    close_fixture(fixture).await;
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

#[tokio::test]
async fn read_group_brokers_every_member_and_returns_source_order() {
    let fixture = mobile_dispatcher_fixture_with_policy(
        "orchestration-read-group",
        "exercise grouped reads",
        Arc::new(FakeMobileBackend::default()),
        None,
        true,
    )
    .await;
    let first_path = std::path::Path::new(&fixture.cwd).join("first.txt");
    let second_path = std::path::Path::new(&fixture.cwd).join("second.txt");
    std::fs::write(&first_path, "first-result\n").expect("write first fixture");
    std::fs::write(&second_path, "second-result\n").expect("write second fixture");
    let script = fs_read_group_script(
        &first_path.to_string_lossy(),
        &second_path.to_string_lossy(),
    );
    let result = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &ItemId::new("outer-read-group-item"),
            "outer-read-group-call",
            "tool_script",
            serde_json::Value::String(script),
            &CancelToken::new(),
        )
        .await
        .expect("execute grouped script");
    let ToolDispatchResult::Completed(result) = result else {
        panic!("grouped script unexpectedly requested approval");
    };
    assert_eq!(
        result.status,
        ToolResultStatus::Completed,
        "{}",
        result.preview
    );
    let terminal = result.orchestration.expect("grouped terminal");
    assert_eq!(terminal.status, ScriptTerminalStatusV1::Completed);
    assert_eq!(terminal.counts.calls, 2);
    assert_eq!(terminal.counts.attempts, 2);
    let values = terminal
        .value
        .expect("grouped exit value")
        .0
        .as_array()
        .expect("all join returns an array")
        .clone();
    assert_eq!(values.len(), 2);
    assert!(
        values[0]["preview"]
            .as_str()
            .unwrap_or_default()
            .contains("first-result")
    );
    assert!(
        values[1]["preview"]
            .as_str()
            .unwrap_or_default()
            .contains("second-result")
    );
    assert_eq!(
        orchestration_journal_counts(&fixture).await,
        (2, 2, 2, 2, 2)
    );
}

#[tokio::test]
async fn hidden_child_tool_is_rejected_before_any_broker_effect() {
    let fixture = mobile_dispatcher_fixture_with_policy(
        "orchestration-hidden-child",
        "exercise hidden child rejection",
        Arc::new(FakeMobileBackend::default()),
        None,
        true,
    )
    .await;
    let result = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &ItemId::new("outer-hidden-item"),
            "outer-hidden-call",
            "tool_script",
            serde_json::Value::String(hidden_mobile_script()),
            &CancelToken::new(),
        )
        .await
        .expect("reject hidden child script");
    let ToolDispatchResult::Completed(result) = result else {
        panic!("hidden child unexpectedly reached approval");
    };
    assert_eq!(result.status, ToolResultStatus::Rejected);
    let terminal = result.orchestration.expect("rejection terminal");
    assert_eq!(terminal.status, ScriptTerminalStatusV1::Rejected);
    assert!(
        terminal
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("wrapper_unavailable")),
        "{terminal:?}"
    );
    assert_eq!(
        orchestration_journal_counts(&fixture).await,
        (0, 0, 0, 0, 0)
    );
}

#[tokio::test]
async fn repeat_safe_retries_use_fresh_attempts_one_deadline_and_durable_backoff() {
    let fixture = mobile_dispatcher_fixture_with_policy(
        "orchestration-retry",
        "exercise bounded retries",
        Arc::new(FakeMobileBackend::default()),
        None,
        true,
    )
    .await;
    let missing = std::path::Path::new(&fixture.cwd).join("missing.txt");
    let started = std::time::Instant::now();
    let result = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &ItemId::new("outer-retry-item"),
            "outer-retry-call",
            "tool_script",
            serde_json::Value::String(retrying_fs_read_script(&missing.to_string_lossy())),
            &CancelToken::new(),
        )
        .await
        .expect("execute retrying script");
    let elapsed = started.elapsed();
    let ToolDispatchResult::Completed(result) = result else {
        panic!("retrying script unexpectedly requested approval");
    };
    assert_eq!(
        result.status,
        ToolResultStatus::Failed,
        "{}",
        result.preview
    );
    let terminal = result.orchestration.expect("retry terminal");
    assert_eq!(terminal.status, ScriptTerminalStatusV1::Failed);
    assert_eq!(terminal.counts.calls, 1);
    assert_eq!(terminal.counts.attempts, 3);
    assert_eq!(terminal.counts.failed, 3);
    assert!(
        elapsed >= std::time::Duration::from_millis(290),
        "{elapsed:?}"
    );
    assert_eq!(
        orchestration_journal_counts(&fixture).await,
        (0, 0, 0, 0, 3),
        "path resolution fails before the effect boundary, but each attempt still settles"
    );

    let clocks = orchestration_activation_clocks(&fixture).await;
    assert_eq!(clocks.len(), 3);
    assert!(clocks.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert!(clocks.iter().all(|clock| clock.1 == clocks[0].1));
    assert!(clocks.iter().all(|clock| clock.2 == clocks[0].2));
}

#[tokio::test]
async fn orchestration_recovery_matrix_never_replays_dispatched_or_effectless_children() {
    assert_recovery_boundary("orch-crash-started", CrashBoundary::Started, false).await;
    assert_recovery_boundary("orch-crash-intent", CrashBoundary::Intent, false).await;
    assert_recovery_boundary("orch-crash-authorized", CrashBoundary::Authorized, false).await;
    assert_recovery_boundary("orch-crash-dispatched", CrashBoundary::Dispatched, false).await;
    assert_recovery_boundary("orch-crash-outcome", CrashBoundary::Outcome, false).await;
    assert_recovery_boundary("orch-crash-result", CrashBoundary::ToolResult, false).await;
    assert_recovery_boundary("orch-crash-actor", CrashBoundary::Started, true).await;
}
