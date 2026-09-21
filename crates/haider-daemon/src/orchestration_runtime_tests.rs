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
use haider_protocol::menu::{AnswerVia, DecisionKind, Menu, MenuAnswer};
use haider_protocol::orchestration::ScriptTerminalStatusV1;
use haider_protocol::pipe::InstructEvidenceRef;
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

fn pending_interaction(action: &str) -> crate::orchestration::PendingCallV1 {
    crate::orchestration::PendingCallV1 {
        slot: 0,
        attempt: 1,
        item_id: "interaction-item".into(),
        call_id: "interaction-call".into(),
        tool: "mobile".into(),
        args: StrictJson(serde_json::json!({"action": action, "x": 1, "y": 1})),
        activation_ref: haider_protocol::pipe::InstructEvidenceRef::new(
            haider_protocol::ids::ArtifactRef::new(format!("blake3:{}", "a".repeat(64))),
            "OrchActivationV1",
            1,
            vec![],
        ),
        started_at_ms: 1,
        deadline_ms: 2,
        started: true,
        approval_waiting: true,
    }
}

fn screenshot_result(byte_len: u64) -> BoundedResult {
    BoundedResult {
        preview: "screenshot".into(),
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: vec![ImageBlockRef {
            artifact: haider_protocol::ids::ArtifactRef::new(format!("blake3:{}", "b".repeat(64))),
            media_type: "image/png".into(),
            width: 1,
            height: 1,
            byte_len,
        }],
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
        orchestration: None,
    }
}

#[test]
fn stale_interaction_observation_rejects_control_before_dispatch() {
    let stale = stale_interaction_control_result(&pending_interaction("tap"), true)
        .expect("stale control must fail closed");
    assert_eq!(stale.status, ToolResultStatus::Failed);
    assert!(stale.images.is_empty());
    assert!(stale_interaction_control_result(&pending_interaction("tap"), false).is_none());
    assert!(stale_interaction_control_result(&pending_interaction("screenshot"), true).is_none());
}

#[test]
fn orchestration_screenshot_retention_caps_exact_encoded_bytes() {
    let mut count = 0;
    let mut bytes = 0;
    let mut exact =
        screenshot_result(haider_protocol::orchestration::ORCHESTRATION_SCREENSHOT_BYTES_MAX);
    assert!(bound_orchestration_screenshot_retention(
        &mut count, &mut bytes, &mut exact,
    ));
    assert_eq!(count, 1);
    assert_eq!(
        bytes,
        haider_protocol::orchestration::ORCHESTRATION_SCREENSHOT_BYTES_MAX
    );

    let mut overflow = screenshot_result(1);
    assert!(!bound_orchestration_screenshot_retention(
        &mut count,
        &mut bytes,
        &mut overflow,
    ));
    assert_eq!(count, 1);
    assert_eq!(
        bytes,
        haider_protocol::orchestration::ORCHESTRATION_SCREENSHOT_BYTES_MAX
    );
    assert_eq!(overflow.status, ToolResultStatus::Failed);
    assert!(overflow.images.is_empty());
}

#[test]
fn orchestration_catalog_matches_initial_profile_and_durable_promotions() {
    let dependencies = DaemonDependencies::default()
        .with_tool_exposure(Some(Vec::new()))
        .with_tool_capability_profile(haider_core::ToolCapabilityProfile::Coding);
    let initial =
        orchestration_entry_child_tools(dependencies.tool_factory.as_ref(), None, None, &[], false);
    assert!(initial.iter().any(|name| name == "fs_read"));
    assert!(initial.iter().any(|name| name == "process_exec"));
    assert!(!initial.iter().any(|name| name == "test_run"));

    let promoted = orchestration_entry_child_tools(
        dependencies.tool_factory.as_ref(),
        None,
        None,
        &["test_run".into()],
        false,
    );
    assert!(promoted.iter().any(|name| name == "test_run"));
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

fn two_process_exec_script(first: &str, second: &str) -> String {
    let entry = registered_tool_by_name("process_exec").expect("process_exec registry entry");
    let wrapper_digest = orchestration_wrapper_digest(entry);
    let factory = BrokerToolFactory::with_mobile_backend(Arc::new(FakeMobileBackend::default()));
    let child_tools = orchestration_entry_child_tools(&factory, None, None, &[], false);
    let catalog_digest =
        orchestration_catalog_digest_for_names(registered_tools(), Some(&child_tools));
    let mut nodes = Vec::new();
    let mut awaits = Vec::new();
    for (ordinal, command) in [first, second].into_iter().enumerate() {
        let value_slot = nodes.len();
        nodes.push(serde_json::json!({
            "slot": value_slot,
            "evidence_type": "OrchValueV1",
            "config": {
                "operator": "literal",
                "type": {"kind":"record","fields":{
                    "command":{"kind":"string","max_bytes":command.len()}
                }},
                "operand_config": {"value":{"command":command}},
                "region": [],
                "origin": [ordinal]
            },
            "ports": []
        }));
        let argument_slot = nodes.len();
        nodes.push(serde_json::json!({
            "slot": argument_slot,
            "evidence_type": "OrchArgumentV1",
            "config": {
                "tool":"process_exec","wrapper_digest":wrapper_digest,
                "region":[],"origin":[ordinal]
            },
            "ports":[{"role":"data","port":"args","source_slot":value_slot,"output":"value"}]
        }));
        let retry_slot = nodes.len();
        nodes.push(serde_json::json!({
            "slot":retry_slot,"evidence_type":"OrchRetryPolicyV1",
            "config":{"max_attempts":1},"ports":[]
        }));
        let ask_slot = nodes.len();
        let call_slot = ask_slot + 1;
        let mut ask_ports = vec![serde_json::json!({
            "role":"data","port":"args","source_slot":argument_slot,"output":"value"
        })];
        if let Some(previous) = awaits.last() {
            ask_ports.push(serde_json::json!({
                "role":"control","port":"after_prior","source_slot":previous,"output":"settled"
            }));
        }
        nodes.push(serde_json::json!({
            "slot":ask_slot,"evidence_type":"OrchAskPauseV1",
            "config":{
                "tool":"process_exec","wrapper_digest":wrapper_digest,
                "owner_call_slot":call_slot,"wait_ms":120000,
                "region":[],"origin":[ordinal]
            },
            "ports":ask_ports
        }));
        nodes.push(serde_json::json!({
            "slot":call_slot,"evidence_type":"OrchCallV1",
            "config":{
                "tool":"process_exec","wrapper_digest":wrapper_digest,
                "region":[],"origin":[ordinal]
            },
            "ports":[
                {"role":"data","port":"args","source_slot":argument_slot,"output":"value"},
                {"role":"control","port":"permit","source_slot":ask_slot,"output":"permit"},
                {"role":"config","port":"retry","source_slot":retry_slot,"output":"policy"}
            ]
        }));
        let await_slot = nodes.len();
        nodes.push(serde_json::json!({
            "slot":await_slot,"evidence_type":"OrchAwaitV1",
            "config":{"on_error":"stop","region":[],"origin":[ordinal]},
            "ports":[{"role":"data","port":"operation","source_slot":call_slot,"output":"operation"}]
        }));
        awaits.push(await_slot);
    }
    let join_slot = nodes.len();
    nodes.push(serde_json::json!({
        "slot":join_slot,"evidence_type":"OrchJoinV1",
        "config":{
            "mode":"all","type":{"kind":"list","item":{
                "kind":"opaque","ref_kind":"tool_result",
                "issuer_version":"1","decoder_version":"1"
            },"max_items":2},
            "omit_inactive":false,"region":[]
        },
        "ports":[
            {"role":"data","port":"first","source_slot":awaits[0],"output":"value"},
            {"role":"data","port":"second","source_slot":awaits[1],"output":"value"}
        ]
    }));
    let exit_slot = nodes.len();
    nodes.push(serde_json::json!({
        "slot":exit_slot,"evidence_type":"OrchExitV1",
        "config":{"mode":"return","region":[]},
        "ports":[{"role":"data","port":"value","source_slot":join_slot,"output":"value"}]
    }));
    serde_json::json!({
        "version":1,
        "transport":haider_protocol::orchestration::ORCHESTRATION_TRANSPORT,
        "catalog_digest":catalog_digest,
        "graph":{
            "kind":"inline","parameters":[],"nodes":nodes,
            "exits":[exit_slot],"read_groups":[]
        },
        "inputs":[]
    })
    .to_string()
}

fn menu_answer(menu: &Menu, decision: DecisionKind) -> MenuAnswer {
    let (index, option) = menu
        .options
        .iter()
        .enumerate()
        .find(|(_, option)| option.decision == Some(decision))
        .expect("permission menu decision");
    MenuAnswer {
        menu: menu.id.clone(),
        option_index: u32::try_from(index).expect("menu option index"),
        option_key: Some(option.key.clone()),
        value: None,
        via: AnswerVia::Rpc,
    }
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

async fn orchestration_child_statuses(fixture: &MobileDispatcherFixture) -> Vec<ToolResultStatus> {
    fixture
        .store
        .read(&fixture.session_id, 0, 4_096)
        .await
        .expect("read child settlements")
        .into_iter()
        .filter_map(|envelope| match envelope.payload.decode_event() {
            Ok(EventPayload::ToolResult { call_id, result }) if call_id.starts_with("orch:") => {
                Some(result.status)
            }
            _ => None,
        })
        .collect()
}

async fn orchestration_admitted_refs(
    fixture: &MobileDispatcherFixture,
) -> (InstructEvidenceRef, InstructEvidenceRef) {
    fixture
        .store
        .read(&fixture.session_id, 0, 4_096)
        .await
        .expect("read script admission")
        .into_iter()
        .find_map(|envelope| {
            let Ok(EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            })) = envelope.payload.decode_event()
            else {
                return None;
            };
            if kind != haider_protocol::orchestration::ORCHESTRATION_SCRIPT_EXTENSION {
                return None;
            }
            Some((
                serde_json::from_value(data.get("shape_ref")?.clone()).ok()?,
                serde_json::from_value(data.get("source_ref")?.clone()).ok()?,
            ))
        })
        .expect("admitted shape/source refs")
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
    let (shape_ref, source_ref) = orchestration_admitted_refs(&fixture).await;
    assert_eq!(
        terminal.shape_digest.as_deref(),
        Some(shape_ref.artifact.as_str()),
        "shape_digest binds the root artifact domain"
    );
    assert_eq!(
        terminal.source_digest.as_deref(),
        Some(source_ref.artifact.as_str()),
        "source_digest binds the concrete script artifact domain"
    );
    assert_ne!(terminal.shape_digest, Some(shape_ref.ledger_digest));
    assert_ne!(terminal.source_digest, Some(source_ref.ledger_digest));

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
async fn runtime_permission_denial_fails_child_and_script_after_prior_effect() {
    let fixture = mobile_dispatcher_fixture_with_policy(
        "orchestration-permission-denial",
        "exercise independent orchestration approvals",
        Arc::new(FakeMobileBackend::default()),
        None,
        false,
    )
    .await;
    let first_marker = std::path::Path::new(&fixture.cwd).join("first-marker.txt");
    let second_marker = std::path::Path::new(&fixture.cwd).join("second-marker.txt");
    let script = two_process_exec_script(
        "printf first > first-marker.txt",
        "printf second > second-marker.txt",
    );
    let item_id = ItemId::new("outer-permission-script-item");

    let first = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &item_id,
            "outer-permission-script-call",
            "tool_script",
            serde_json::Value::String(script.clone()),
            &CancelToken::new(),
        )
        .await
        .expect("open first child approval");
    let ToolDispatchResult::ApprovalRequired(first_menu) = first else {
        panic!("first process child must ask independently");
    };
    assert!(!first_marker.exists());
    assert!(!second_marker.exists());
    fixture
        .dispatcher
        .resolve_approval(
            &first_menu,
            &menu_answer(&first_menu, DecisionKind::AllowOnce),
        )
        .await
        .expect("approve first child once");

    let second = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &item_id,
            "outer-permission-script-call",
            "tool_script",
            serde_json::Value::String(script.clone()),
            &CancelToken::new(),
        )
        .await
        .expect("execute first child and open second approval");
    let ToolDispatchResult::ApprovalRequired(second_menu) = second else {
        panic!("second process child must ask independently");
    };
    assert_ne!(first_menu.id, second_menu.id);
    assert_eq!(
        std::fs::read_to_string(&first_marker).expect("first child marker"),
        "first"
    );
    assert!(!second_marker.exists());
    fixture
        .dispatcher
        .resolve_approval(
            &second_menu,
            &menu_answer(&second_menu, DecisionKind::RejectOnce),
        )
        .await
        .expect("deny second child once");

    let denied = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &item_id,
            "outer-permission-script-call",
            "tool_script",
            serde_json::Value::String(script),
            &CancelToken::new(),
        )
        .await
        .expect("settle denied script");
    let ToolDispatchResult::Completed(denied) = denied else {
        panic!("denied child must terminate the script");
    };
    assert_eq!(
        denied.status,
        ToolResultStatus::Failed,
        "{}",
        denied.preview
    );
    let terminal = denied.orchestration.expect("permission denial terminal");
    assert_eq!(terminal.status, ScriptTerminalStatusV1::Failed);
    assert_eq!(terminal.reason_code.as_deref(), Some("permission_denied"));
    assert_eq!(terminal.counts.completed, 1);
    assert_eq!(terminal.counts.failed, 1);
    assert_eq!(terminal.counts.rejected, 0);
    assert_eq!(
        orchestration_child_statuses(&fixture).await,
        [ToolResultStatus::Completed, ToolResultStatus::Failed]
    );
    let (_, _, dispatched, outcomes, child_results) = orchestration_journal_counts(&fixture).await;
    assert_eq!(dispatched, 1, "only the approved child may dispatch");
    assert_eq!(outcomes, 1, "only the approved child may have an outcome");
    assert_eq!(child_results, 2);
    assert_eq!(
        std::fs::read_to_string(&first_marker).expect("first child still executed once"),
        "first"
    );
    assert!(!second_marker.exists(), "denied child must never dispatch");
    close_fixture(fixture).await;
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
