#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_protocol::tool::DispatchMode;
use haider_tools::{EffectBroker, JournalSink, PermissionPolicy, ToolResult};
use haider_tools::{SpawnSubagent, spawn_subagent_manifest};

#[derive(Default)]
struct RecordingJournal {
    payloads: Vec<EventPayload>,
}

#[async_trait::async_trait]
impl JournalSink for RecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads.push(payload);
        Ok(())
    }
}

/// MUTATION CHECK: advertise spawn as an awaited tool or omit AgentSpawn.
/// Expected runtime failure: the manifest no longer selects deferred parent
/// parking or the effect broker cannot authorize the durable spawn boundary.
#[test]
fn spawn_manifest_is_deferred_and_agent_spawn_effectful() {
    let manifest = spawn_subagent_manifest();
    assert_eq!(manifest.name, "spawn_subagent");
    assert_eq!(manifest.dispatch, DispatchMode::Deferred);
    assert_eq!(manifest.effects, vec![EffectClass::AgentSpawn]);
    assert_eq!(
        manifest.input_schema["required"],
        serde_json::json!(["task", "prompt"])
    );
}

/// MUTATION CHECK: stop trimming/bounding either user-controlled string.
/// Expected runtime failure: blank labels/prompts or oversized prompt data
/// cross the durable delegation and provider boundaries.
#[test]
fn spawn_arguments_are_trimmed_and_bounded() {
    let request = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "  tests  ",
        "prompt": "  run them  "
    }))
    .expect("valid request");
    assert_eq!(request.task, "tests");
    assert_eq!(request.prompt, "run them");
    assert!(SpawnSubagent::from_tool_args(serde_json::json!({"task":" ","prompt":"run"})).is_err());
    assert!(
        SpawnSubagent::from_tool_args(
            serde_json::json!({"task":"tests","prompt":"x".repeat(32 * 1024 + 1)})
        )
        .is_err()
    );
}

/// MUTATION CHECK: keep the AgentSpawn effect dispatched until simulated
/// child execution/report collection. Expected runtime failure: the journal
/// snapshot lacks `Outcome::Ok` at the durable-establishment boundary.
#[tokio::test]
async fn spawn_effect_terminalizes_at_establishment_not_child_completion() {
    let mut broker = EffectBroker::new_at(
        Box::new(RecordingJournal::default()),
        env!("CARGO_MANIFEST_DIR"),
        SessionId::new("spawn-effect-session"),
        1,
        1,
    )
    .expect("broker");
    let mut policy = PermissionPolicy::default();
    policy.allow(EffectClass::AgentSpawn);
    let request = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "tests",
        "prompt": "run them"
    }))
    .expect("request");
    let intent = broker
        .begin_agent_spawn(&request, &policy)
        .await
        .expect("spawn dispatched");

    // This call represents durable child session + link establishment. No
    // child-completion signal exists in this fixture.
    broker
        .finish_agent_spawn(&intent, Ok(()))
        .await
        .expect("spawn effect terminal");
    let phases = broker.journal_snapshot();
    assert!(matches!(
        phases.last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Ok,
            ..
        })
    ));
    broker.close().await.expect("close broker");
}
