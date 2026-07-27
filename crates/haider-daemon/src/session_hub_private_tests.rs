//! Private session-hub accounting tests.

#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{AgentId, BranchId, EventId, RunId};

/// MUTATION CHECK: remove any owned ID charge from
/// `envelope_weight_bytes` (for example `branch_id`). Expected failure: the
/// estimator falls below the explicit fixed-value-plus-owned-strings size.
#[test]
fn envelope_weight_charges_every_large_owned_id_string() {
    let large = |label: &str| format!("{label}-{}", "x".repeat(16 * 1024));
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(large("event")),
        seq: 1,
        session_id: SessionId::new(large("session")),
        branch_id: Some(BranchId::new(large("branch"))),
        run_id: Some(RunId::new(large("run"))),
        agent_id: Some(AgentId::new(large("agent"))),
        device_id: DeviceId::new(large("device")),
        authority_epoch: 2,
        worker_generation: 3,
        causation_id: Some(EventId::new(large("causation"))),
        correlation_id: Some(EventId::new(large("correlation"))),
        committed_at_ms: 4,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::Value::Null,
    };
    let owned_string_bytes = envelope
        .event_id
        .as_str()
        .len()
        .saturating_add(envelope.session_id.as_str().len())
        .saturating_add(
            envelope
                .branch_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .run_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .agent_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(envelope.device_id.as_str().len())
        .saturating_add(
            envelope
                .causation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .correlation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        );
    let real_owned_lower_bound =
        std::mem::size_of::<RawEnvelope>().saturating_add(owned_string_bytes);

    assert!(
        envelope_weight_bytes(&envelope) >= real_owned_lower_bound,
        "every variable-length envelope field must be charged"
    );
}
