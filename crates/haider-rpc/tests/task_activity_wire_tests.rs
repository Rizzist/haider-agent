#![allow(clippy::expect_used)]

mod common;
use haider_rpc::{
    CommandDynamicSlotsWire, CommandOwnershipWire, ResponseBody, WireFrame, command_catalog_items,
    ws_codec,
};
use serde_json::Value;

#[test]
fn activity_wire_golden_round_trips_json_and_msgpack_without_new_methods() {
    let goldens: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/task_activity_wire_v1.json")).expect("fixture");
    for golden in goldens {
        let frame: WireFrame = serde_json::from_value(golden.clone()).expect("frame");
        assert_eq!(serde_json::to_value(&frame).expect("encode"), golden);
        let json = ws_codec::encode(&frame, 4096).expect("JSON");
        assert_eq!(ws_codec::decode(&json, 4096).expect("JSON decode"), frame);
        let binary = ws_codec::encode_binary(&frame, 4096).expect("msgpack");
        assert_eq!(
            ws_codec::decode_binary(&binary, 4096).expect("msgpack decode"),
            frame
        );
    }
    let methods: Value =
        serde_json::from_str(include_str!("fixtures/client_contract_methods_v1.json"))
            .expect("method fixture");
    assert!(methods.to_string().contains("session.observe"));
    assert!(!methods.to_string().contains("task.list"));
}

#[test]
fn activity_fields_are_absent_on_legacy_digests_and_optional_for_legacy_readers() {
    let digest = common::transcript()
        .into_iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body: ResponseBody::SessionObserve { digest },
                ..
            } => Some(digest),
            _ => None,
        })
        .expect("old observe fixture");
    assert_eq!(digest.tasks, None);
    assert_eq!(digest.shells, None);
    let value = serde_json::to_value(&digest).expect("digest");
    assert!(value.get("tasks").is_none());
    assert!(value.get("shells").is_none());
    let old: haider_rpc::ObserveSubagentWire = serde_json::from_value(serde_json::json!({
        "agent_id":"old-child", "task":"legacy", "state":"thinking"
    }))
    .expect("legacy child");
    assert_eq!(old.agent_type, None);
    #[derive(serde::Deserialize)]
    struct LegacyDigest {
        session_id: String,
    }
    let goldens: Value =
        serde_json::from_str(include_str!("fixtures/task_activity_wire_v1.json")).expect("fixture");
    let legacy: LegacyDigest = serde_json::from_value(goldens[1]["body"]["digest"].clone())
        .expect("old reader ignores new fields");
    assert_eq!(legacy.session_id, "session-1");
}

#[test]
fn activity_commands_are_unique_client_views_with_typed_argument_hints() {
    let specs = [
        ("collapse", "[all|expand|next|prev]", true),
        ("verbosity", "[quiet|default|verbose]", false),
        ("tasks", "", true),
    ];
    for (name, hint, session_only) in specs {
        let entries: Vec<_> = haider_rpc::COMMANDS
            .iter()
            .filter(|spec| spec.name == name)
            .collect();
        assert_eq!(entries.len(), 1, "{name}");
        assert_eq!(entries[0].arg_hint, hint);
        assert_eq!(entries[0].session_only, session_only);
        assert_eq!(entries[0].ownership, CommandOwnershipWire::ClientView);
        for in_session in [false, true] {
            let items = command_catalog_items("", in_session, &CommandDynamicSlotsWire::default());
            assert_eq!(
                items.iter().any(|item| item.name.as_deref() == Some(name)),
                in_session || !session_only
            );
        }
    }
}
