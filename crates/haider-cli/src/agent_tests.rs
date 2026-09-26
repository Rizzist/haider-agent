use super::*;

#[test]
fn agent_spawn_derives_output_budget_for_a_small_limit_model() {
    let spec = AgentSpawnSpecV1 {
        task: "test".into(),
        prompt: "hello".into(),
        provider: Some("openai".into()),
        model: Some("gpt-4o".into()),
        agent_type: None,
        workflow: None,
        workflow_trigger: None,
    };
    let request = agent_session_create_request("agent-test", "/tmp".into(), &spec);
    assert!(
        matches!(request, RequestBody::SessionCreateWithPermissionOverrides {
        max_tokens: 0, provider, model, ..
    } if provider == "openai" && model == "gpt-4o")
    );
}

#[test]
fn workflow_run_uses_the_same_derived_budget() {
    let spec = AgentSpawnSpecV1 {
        task: "workflow".into(),
        prompt: "hello".into(),
        provider: Some("deepseek".into()),
        model: Some("deepseek-reasoner".into()),
        agent_type: None,
        workflow: Some("audit".into()),
        workflow_trigger: Some("manual".into()),
    };
    assert!(matches!(
        agent_session_create_request("workflow-test", "/tmp".into(), &spec),
        RequestBody::SessionCreateWithPermissionOverrides { max_tokens: 0, .. }
    ));
}

#[test]
#[allow(clippy::expect_used)]
fn agent_list_human_output_is_the_fleet_tree_with_activity() {
    let snapshot = serde_json::json!({
        "session_id": "parent",
        "generated_at_ms": 10,
        "node_limit": 512,
        "depth_limit": 32,
        "roots": [{
            "agent_id": "agent-a",
            "session_id": "child-a",
            "callsign": "Amina",
            "task": "patch the parser",
            "depth": 1,
            "parent_session_id": "parent",
            "state": "live",
            "activity": "edit",
            "activities": ["edit", "read"]
        }],
        "rollup": {
            "node_count": 1,
            "states": {"queued": 0, "live": 1, "waiting": 0, "done": 0, "failed": 0, "cancelled": 0},
            "max_depth": 1,
            "metrics": {"elapsed_ms": 0, "tool_attempts": 0},
            "metrics_complete": false,
            "complete": true
        },
        "truncated": false
    });
    let document = serde_json::json!({"schema": "haider.agent.list.v1", "ok": true, "result": snapshot, "error": null});
    let text = agent_list_human_text("agent", "list", &document).expect("human fleet text");
    assert!(text.starts_with("fleet of 1"), "{text}");
    assert!(text.contains("Amina — patch the parser"), "{text}");
    assert!(
        text.contains("\u{270F}\u{FE0F} Amina") || text.contains("[edit] Amina"),
        "{text}"
    );
    // Other verbs, errors and undecodable results keep the JSON fallback.
    assert!(agent_list_human_text("agent", "wait", &document).is_none());
    let failed = serde_json::json!({"ok": false, "result": null, "error": {"code": "x"}});
    assert!(agent_list_human_text("agent", "list", &failed).is_none());
}
