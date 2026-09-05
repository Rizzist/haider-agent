#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::ids::SessionId;
use haider_protocol::tool::DispatchMode;
use haider_tools::{EffectBroker, EffectOperation, JournalSink, PermissionPolicy, ToolResult};
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
    let model_description = manifest.input_schema["properties"]["model"]["description"]
        .as_str()
        .expect("model description");
    assert_eq!(
        model_description,
        "Optional model for the child; omitted, the child inherits this session's current model. A bare selector matches case-insensitively and ignores '-', '_', '.', and whitespace; literal exact matches win. Call list_models to inspect valid model/provider pairs."
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

#[test]
fn child_request_budget_is_validated_and_preserved_in_spawn_arguments() {
    let request = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "long task", "prompt": "continue to completion",
        "request_budget": {"tranche": 40, "hard_cap": 96}
    }))
    .expect("valid child budget");
    assert_eq!(
        request.arguments().expect("arguments")["request_budget"],
        serde_json::json!({"tranche": 40, "hard_cap": 96})
    );
    for (tranche, hard_cap) in [(0, 64), (32, 0), (65, 64)] {
        assert!(
            SpawnSubagent::from_tool_args(serde_json::json!({
                "task": "task", "prompt": "prompt",
                "request_budget": {"tranche": tranche, "hard_cap": hard_cap}
            }))
            .is_err()
        );
    }
    assert_eq!(
        spawn_subagent_manifest().input_schema["properties"]["request_budget"]["required"],
        serde_json::json!(["tranche", "hard_cap"])
    );
}

#[test]
fn m2e_legacy_spawn_arguments_remain_byte_for_byte_plain() {
    // MUTATION CHECK: serialize any default workflow field. Expected failure:
    // the frozen legacy argument value gains a key below.
    let request = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "tests",
        "prompt": "run them"
    }))
    .expect("legacy request");
    assert_eq!(
        request.arguments().expect("arguments"),
        serde_json::json!({"task":"tests","prompt":"run them"})
    );
    assert!(request.workflow.is_none());
    assert!(request.workflow_trigger.is_none());
    assert!(request.parent_slot.is_none());
    assert!(!request.workflow_author);
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

/// C2 MUTATION CHECK: drop the agent_type plumbing or its selector bounds.
/// Expected RUNTIME failure below.
#[test]
fn agent_type_rides_the_spawn_args() {
    let request = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "thumbnail",
        "prompt": "make the cover",
        "agent_type": "thumbnailer",
    }))
    .expect("typed spawn parses");
    assert_eq!(request.agent_type.as_deref(), Some("thumbnailer"));
    // Absent stays None; an over-long selector rejects.
    let bare = SpawnSubagent::from_tool_args(serde_json::json!({
        "task": "t",
        "prompt": "p",
    }))
    .expect("bare spawn parses");
    assert!(bare.agent_type.is_none());
    let long = "x".repeat(400);
    assert!(
        SpawnSubagent::from_tool_args(serde_json::json!({
            "task": "t",
            "prompt": "p",
            "agent_type": long,
        }))
        .is_err()
    );
}
