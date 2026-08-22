#![allow(clippy::expect_used)]

use haider_rpc::{
    CommandDynamicSlotsWire, CommandId, CommandInvokeOutcomeWire, CommandOwnershipWire,
    RequestBody, RequestId, ResponseBody, WireFrame, command_catalog_items,
};

/// MUTATION CHECK: replace the `session_only` filter in `palette_items` with
/// an unconditional true predicate. Runtime failure: the launcher catalog
/// advertises `/compact`, which cannot be invoked without a session.
#[test]
fn command_catalog_reflects_session_context_and_dynamic_slots() {
    let slots = CommandDynamicSlotsWire {
        models: vec![("future-model".into(), "served model".into())],
        custom_commands: vec![("local-workflow".into(), "client prompt".into())],
        ..CommandDynamicSlotsWire::default()
    };
    let launcher = command_catalog_items("", false, &slots);
    let session = command_catalog_items("", true, &slots);
    assert!(
        !launcher
            .iter()
            .any(|item| item.name.as_deref() == Some("compact"))
    );
    assert!(
        session
            .iter()
            .any(|item| item.name.as_deref() == Some("compact"))
    );

    let model_rows = command_catalog_items("model ", true, &slots);
    assert_eq!(model_rows.len(), 1);
    assert_eq!(model_rows[0].value.as_deref(), Some("future-model"));
    assert_eq!(
        model_rows[0].ownership,
        CommandOwnershipWire::DaemonOperation
    );
    let launcher_model_rows = command_catalog_items("model ", false, &slots);
    assert_eq!(launcher_model_rows.len(), 1);
    assert_eq!(
        launcher_model_rows[0].ownership,
        CommandOwnershipWire::ClientView,
        "launcher model selection is client-local view identity"
    );
    assert!(
        launcher
            .iter()
            .all(|item| item.ownership != CommandOwnershipWire::Unknown),
        "every served row has explicit ownership"
    );
}

/// MUTATION CHECK: remove either `in_session` guard from the provider or
/// effort argument arm. Expected runtime failure: that launcher query returns
/// a session-only daemon operation.
#[test]
fn launcher_argument_queries_hide_session_only_operations() {
    let slots = CommandDynamicSlotsWire {
        providers: vec![("future-provider".into(), "served provider".into())],
        efforts: vec![("high".into(), "high effort".into())],
        ..CommandDynamicSlotsWire::default()
    };
    for query in ["provider ", "effort "] {
        let rows = command_catalog_items(query, false, &slots);
        assert!(
            rows.is_empty(),
            "launcher query {query:?} leaked session-only rows: {rows:?}"
        );
    }
    assert_eq!(command_catalog_items("provider ", true, &slots).len(), 1);
    assert_eq!(command_catalog_items("effort ", true, &slots).len(), 1);
}

/// MUTATION CHECK: change one client-routed/stub command to
/// `session_operation_cmd` without adding its command-door dispatcher arm.
/// Runtime failure: the operation-name pin grows, exposing an advertised
/// daemon operation that `command.invoke` cannot perform or park.
#[test]
fn every_advertised_daemon_operation_has_a_command_door_route() {
    let operations: Vec<_> = haider_rpc::COMMANDS
        .iter()
        .filter(|spec| spec.ownership == CommandOwnershipWire::DaemonOperation)
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        operations,
        ["model", "provider", "effort", "fast", "compact", "rename"]
    );
}

/// MUTATION CHECK: change the shared `help` registry constructor from
/// `client_cmd` to `operation_cmd`. Runtime failure: a daemon command palette
/// tells clients to send a request that would claim control of their help UI.
#[test]
fn help_is_explicitly_client_owned() {
    let items = command_catalog_items("help", false, &CommandDynamicSlotsWire::default());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name.as_deref(), Some("help"));
    assert_eq!(items[0].ownership, CommandOwnershipWire::ClientView);
}

/// MUTATION CHECK: remove either enum's `serde(other)` arm. Runtime failure:
/// an older client rejects a newer daemon catalog/result instead of retaining
/// an honest, non-executable `Unknown` value.
#[test]
fn future_command_kinds_decode_as_unknown_without_action_semantics() {
    let owner: CommandOwnershipWire =
        serde_json::from_str(r#""future_owner""#).expect("future owner decodes");
    assert_eq!(owner, CommandOwnershipWire::Unknown);
    let outcome: CommandInvokeOutcomeWire =
        serde_json::from_str(r#"{"kind":"future_result","action":"rename"}"#)
            .expect("future result decodes");
    assert!(matches!(outcome, CommandInvokeOutcomeWire::Unknown));
}

/// MUTATION CHECK: make `session_id` or empty `slots` serialize
/// unconditionally. Runtime failure: the exact additive v1 golden grows
/// mandatory fields and older peers no longer see the omission-compatible
/// request shapes.
#[test]
fn command_door_request_response_shapes_are_additive_and_golden() {
    let list = WireFrame::Request {
        request_id: RequestId::new("list-1"),
        body: RequestBody::CommandList {
            query: String::new(),
            in_session: false,
            slots: CommandDynamicSlotsWire::default(),
        },
    };
    assert_eq!(
        serde_json::to_string(&list).expect("list serializes"),
        r#"{"v":1,"kind":"request","request_id":"list-1","body":{"method":"command.list","query":"","in_session":false}}"#
    );

    let invoke = WireFrame::Request {
        request_id: RequestId::new("invoke-1"),
        body: RequestBody::CommandInvoke {
            command_id: CommandId::new("command-1"),
            command: "/help".into(),
            session_id: None,
        },
    };
    assert_eq!(
        serde_json::to_string(&invoke).expect("invoke serializes"),
        r#"{"v":1,"kind":"request","request_id":"invoke-1","body":{"method":"command.invoke","command_id":"command-1","command":"/help"}}"#
    );

    let response = WireFrame::Response {
        request_id: RequestId::new("invoke-1"),
        body: ResponseBody::CommandInvoke {
            outcome: CommandInvokeOutcomeWire::ClientOwned {
                command: "help".into(),
            },
        },
    };
    assert_eq!(
        serde_json::to_string(&response).expect("response serializes"),
        r#"{"v":1,"kind":"response","request_id":"invoke-1","body":{"method":"command.invoke","outcome":{"kind":"client_owned","command":"help"}}}"#
    );

    let future = r#"{"v":1,"kind":"request","request_id":"list-future","body":{"method":"command.list","query":"","in_session":false,"future":9,"slots":{"future_slots":[1]}}}"#;
    assert!(matches!(
        serde_json::from_str::<WireFrame>(future).expect("future fields decode"),
        WireFrame::Request {
            body: RequestBody::CommandList { .. },
            ..
        }
    ));
}
