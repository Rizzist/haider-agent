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
