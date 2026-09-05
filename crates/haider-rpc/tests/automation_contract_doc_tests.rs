#![allow(clippy::expect_used)]

use haider_rpc::{DaemonCachingWire, RequestBody, ResponseBody, WireFrame};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const AUTOMATION_CONTRACT: &str = include_str!("../../../docs/automation-contract-v1.md");
const WIRE_TRANSCRIPT: &str = include_str!("fixtures/wire_transcript.json");
const METHOD_MATRIX: &str = include_str!("fixtures/client_contract_methods_v1.json");
const CLI_STATUS: &str = include_str!("../../haider-cli/tests/fixtures/observe_status.json");

fn json_examples() -> Vec<(usize, &'static str, String)> {
    let lines = AUTOMATION_CONTRACT.lines().collect::<Vec<_>>();
    let mut examples = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(tag) = lines[index].strip_prefix("```json") else {
            index += 1;
            continue;
        };
        let tag = tag.trim();
        assert!(
            !tag.is_empty(),
            "JSON fence at line {} must name its real frame/body/value type",
            index + 1
        );
        let start_line = index + 1;
        index += 1;
        let mut body = String::new();
        while index < lines.len() && lines[index] != "```" {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(lines[index]);
            index += 1;
        }
        assert!(
            index < lines.len(),
            "unterminated JSON fence starting at line {start_line}"
        );
        examples.push((start_line, tag, body));
        index += 1;
    }
    examples
}

fn wire_fixture_values() -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(WIRE_TRANSCRIPT)
        .expect("wire transcript JSON")
        .into_iter()
        .map(|row| {
            serde_json::from_str(
                row.get("ws_body")
                    .and_then(Value::as_str)
                    .expect("wire transcript ws_body"),
            )
            .expect("wire transcript frame JSON")
        })
        .collect()
}

fn method_fixture_values(field: &str) -> Vec<Value> {
    let matrix: Value = serde_json::from_str(METHOD_MATRIX).expect("method matrix JSON");
    matrix["methods"]
        .as_array()
        .expect("method matrix methods")
        .iter()
        .map(|row| row[field].clone())
        .collect()
}

fn assert_wire_variant(tag: &str, frame: &WireFrame, line: usize) {
    let matches_tag = matches!(
        (tag, frame),
        ("wire.hello", WireFrame::Hello(_))
            | ("wire.welcome", WireFrame::Welcome(_))
            | ("wire.request", WireFrame::Request { .. })
            | ("wire.response", WireFrame::Response { .. })
            | ("wire.event", WireFrame::Event { .. })
            | ("wire.attach_caught_up", WireFrame::AttachCaughtUp { .. })
            | ("wire.menu_answer", WireFrame::MenuAnswer { .. })
    );
    assert!(
        matches_tag,
        "{tag} fence at line {line} decoded to {frame:?}"
    );
}

fn method_name<'a>(tag: &str, value: &'a Value) -> Option<&'a str> {
    match tag {
        "wire.request" | "wire.response" => value["body"]["method"].as_str(),
        "body.request" | "body.response" => value["method"].as_str(),
        _ => None,
    }
}

fn assert_catalog_coverage(
    tags: &BTreeSet<&str>,
    request_methods: &BTreeSet<String>,
    response_methods: &BTreeSet<String>,
) {
    for tag in [
        "wire.hello",
        "wire.welcome",
        "wire.event",
        "wire.attach_caught_up",
        "wire.menu_answer",
        "value.daemon_caching",
    ] {
        assert!(tags.contains(tag), "automation contract is missing {tag}");
    }

    for method in [
        "session.create",
        "session.list",
        "session.list_watch",
        "session.fork",
        "session.select_model",
        "session.seen",
        "session.diagnostic",
        "turn.submit",
        "turn.cancel",
        "agent.message",
        "agent.cancel",
        "session.observe",
        "daemon.shutdown",
        "status.snapshot",
        "command.list",
        "command.invoke",
    ] {
        assert!(
            request_methods.contains(method),
            "automation contract is missing the {method} request example"
        );
        assert!(
            response_methods.contains(method),
            "automation contract is missing the {method} response example"
        );
    }
    assert!(
        response_methods.contains("menu.answer"),
        "automation contract is missing the menu.answer response example"
    );

    for narrative in [
        "`queue.steer`",
        "`queue.subturn`",
        "`queue.turn`",
        "haider.sessions.ready.v1",
        "haider.session_recovery.v1",
    ] {
        assert!(
            AUTOMATION_CONTRACT.contains(narrative),
            "automation contract is missing the {narrative} catalog entry"
        );
    }
}

fn assert_correlated_wire_examples(
    request_ids: &BTreeMap<String, String>,
    response_ids: &BTreeMap<String, String>,
) {
    for method in [
        "session.create",
        "session.list",
        "session.fork",
        "turn.submit",
        "turn.cancel",
        "agent.message",
        "agent.cancel",
        "session.observe",
    ] {
        assert_eq!(
            request_ids.get(method),
            response_ids.get(method),
            "the full-frame {method} examples must correlate by request_id"
        );
    }
}

#[test]
fn every_automation_contract_json_example_decodes_and_matches_a_golden() {
    let examples = json_examples();
    assert_eq!(
        examples.len(),
        40,
        "the method catalog JSON example inventory changed"
    );

    let wire_goldens = wire_fixture_values();
    let request_goldens = method_fixture_values("request");
    let response_goldens = method_fixture_values("response");
    let status_golden: Value = serde_json::from_str(CLI_STATUS).expect("CLI status golden JSON");
    let mut tags = BTreeSet::new();
    let mut request_methods = BTreeSet::new();
    let mut response_methods = BTreeSet::new();
    let mut wire_request_ids = BTreeMap::new();
    let mut wire_response_ids = BTreeMap::new();

    for (line, tag, source) in examples {
        tags.insert(tag);
        let value: Value = serde_json::from_str(&source)
            .unwrap_or_else(|error| panic!("invalid JSON in {tag} fence at line {line}: {error}"));
        if let Some(method) = method_name(tag, &value) {
            match tag {
                "wire.request" | "body.request" => {
                    request_methods.insert(method.to_owned());
                    if tag == "wire.request" {
                        wire_request_ids.insert(
                            method.to_owned(),
                            value["request_id"]
                                .as_str()
                                .expect("wire request request_id")
                                .to_owned(),
                        );
                    }
                }
                "wire.response" | "body.response" => {
                    response_methods.insert(method.to_owned());
                    if tag == "wire.response" {
                        wire_response_ids.insert(
                            method.to_owned(),
                            value["request_id"]
                                .as_str()
                                .expect("wire response request_id")
                                .to_owned(),
                        );
                    }
                }
                _ => {}
            }
        }
        match tag {
            tag if tag.starts_with("wire.") => {
                let frame: WireFrame =
                    serde_json::from_value(value.clone()).unwrap_or_else(|error| {
                        panic!("invalid WireFrame in {tag} fence at line {line}: {error}")
                    });
                assert_wire_variant(tag, &frame, line);
                assert!(
                    wire_goldens.contains(&value),
                    "{tag} fence at line {line} is not copied from wire_transcript.json"
                );
            }
            "body.request" => {
                let _: RequestBody =
                    serde_json::from_value(value.clone()).unwrap_or_else(|error| {
                        panic!("invalid RequestBody fence at line {line}: {error}")
                    });
                assert!(
                    request_goldens.contains(&value),
                    "request body at line {line} is not copied from client_contract_methods_v1.json"
                );
            }
            "body.response" => {
                let _: ResponseBody =
                    serde_json::from_value(value.clone()).unwrap_or_else(|error| {
                        panic!("invalid ResponseBody fence at line {line}: {error}")
                    });
                assert!(
                    response_goldens.contains(&value),
                    "response body at line {line} is not copied from client_contract_methods_v1.json"
                );
            }
            "value.daemon_caching" => {
                let caching: DaemonCachingWire = serde_json::from_value(value.clone())
                    .unwrap_or_else(|error| {
                        panic!("invalid DaemonCachingWire fence at line {line}: {error}")
                    });
                assert_eq!(
                    serde_json::to_value(caching).expect("caching wire roundtrip"),
                    value,
                    "caching declaration at line {line} must preserve every typed field"
                );
                assert_eq!(
                    value, status_golden["daemon"]["caching"],
                    "caching declaration at line {line} must match observe_status.json"
                );
            }
            other => panic!("unsupported JSON fence tag {other} at line {line}"),
        }
    }

    assert_catalog_coverage(&tags, &request_methods, &response_methods);
    assert_correlated_wire_examples(&wire_request_ids, &wire_response_ids);
}
