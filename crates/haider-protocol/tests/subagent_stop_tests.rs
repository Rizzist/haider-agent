#![allow(clippy::expect_used)]
//! Subagent stop facts (owner decision 2026-09-25). Moved out of the source
//! module per CONVENTIONS rule 1.

use haider_protocol::agent::ReportVerification;
use haider_protocol::ids::{AgentId, RunId, SessionId};
use haider_protocol::subagent_stop::{
    DetachedChildResultV1, DetachedChildStateV1, DetachedDeliveryPhaseV1, DetachedDeliveryV1,
    DetachedReasonV1, StillRunningAgentV1, SubagentsStillRunningV1, bounded_summary,
};

#[test]
fn marker_round_trips_through_extension_item() {
    let marker = SubagentsStillRunningV1 {
        parent_session_id: SessionId::new("s"),
        stopped_run_id: RunId::new("r"),
        still_running: 2,
        agents: vec![StillRunningAgentV1 {
            agent: AgentId::new("a"),
            task: "t".into(),
            result_pending: true,
        }],
        agents_truncated: false,
    };
    let item = marker.to_extension_item().expect("encode");
    assert_eq!(
        SubagentsStillRunningV1::from_extension_item(&item),
        Some(marker.clone())
    );
    assert!(
        marker
            .summary()
            .starts_with("main thread stopped — 2 subagents still running")
    );
    assert!(
        marker
            .model_note()
            .contains("haider agent cancel s --all --json")
    );
}

fn detached(reason: DetachedReasonV1) -> DetachedChildResultV1 {
    DetachedChildResultV1 {
        agent: AgentId::new("a"),
        task: "t".into(),
        stopped_run_id: RunId::new("r"),
        state: DetachedChildStateV1::Done,
        verified: ReportVerification::Unverified,
        summary: "ok".into(),
        summary_truncated: false,
        reason,
    }
}

#[test]
fn detached_result_round_trips_and_bounds_summary() {
    let (summary, truncated) = bounded_summary("ééé", 3);
    assert_eq!((summary.as_str(), truncated), ("é", true));
    let result = detached(DetachedReasonV1::MainStopped);
    let item = result.to_extension_item().expect("encode");
    assert_eq!(
        DetachedChildResultV1::from_extension_item(&item),
        Some(result.clone())
    );
    assert!(result.model_note().contains("state: done"));
    assert!(result.model_note().contains("had been stopped"));
}

/// The default (main-stopped) reason keeps the pre-existing wire shape; an
/// observation-window delivery says why it arrived detached.
#[test]
fn detached_result_reason_is_additive_and_worded() {
    let stopped = serde_json::to_value(detached(DetachedReasonV1::MainStopped)).expect("json");
    assert!(stopped.get("reason").is_none(), "{stopped}");
    let legacy: DetachedChildResultV1 = serde_json::from_value(stopped).expect("legacy decode");
    assert_eq!(legacy.reason, DetachedReasonV1::MainStopped);
    let observed = detached(DetachedReasonV1::ObservationEnded);
    let value = serde_json::to_value(&observed).expect("json");
    assert_eq!(value["reason"], "observation_ended");
    assert!(observed.model_note().contains("wait_ms"));
    assert!(
        observed
            .summary_line()
            .contains("after its observation window ended")
    );
}

/// Delivery obligations are their own per-child facts, independent of the
/// bounded display marker.
#[test]
fn delivery_obligation_round_trips_and_ignores_other_extensions() {
    let owed = DetachedDeliveryV1 {
        agent: AgentId::new("a"),
        phase: DetachedDeliveryPhaseV1::Owed,
        reason: DetachedReasonV1::ObservationEnded,
    };
    let item = owed.to_extension_item().expect("encode");
    assert_eq!(DetachedDeliveryV1::from_extension_item(&item), Some(owed));
    let marker = SubagentsStillRunningV1 {
        parent_session_id: SessionId::new("s"),
        stopped_run_id: RunId::new("r"),
        still_running: 1,
        agents: Vec::new(),
        agents_truncated: false,
    }
    .to_extension_item()
    .expect("marker");
    assert_eq!(DetachedDeliveryV1::from_extension_item(&marker), None);
    let settled: DetachedDeliveryV1 = serde_json::from_value(serde_json::json!({
        "agent": "a", "phase": "settled"
    }))
    .expect("reason defaults");
    assert_eq!(settled.reason, DetachedReasonV1::MainStopped);
}
