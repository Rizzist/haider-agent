//! The event envelope — one event stream, many encodings (Thesis 1).
//!
//! Every committed fact rides this envelope. State is a deterministic
//! projection of committed envelopes; subscribers resume from `seq`.
//!
//! Forward-compat policy (frozen): readers MUST tolerate unknown payload kinds
//! by falling back to [`RawEnvelope`] (same envelope fields, payload kept as
//! raw JSON). Writers never remove or re-type existing fields — schema changes
//! bump `schema_version` with an upcaster and golden old/new fixtures.

use crate::ids::{AgentId, BranchId, DeviceId, EventId, RunId, SessionId};
use serde::{Deserialize, Serialize};

/// Current envelope schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Where an event is rendered (§6.1: three surfaces, never conflated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTargets {
    pub ui: bool,
    pub durable: bool,
    pub prompt: PromptRender,
}

/// How (if at all) the prompt compiler may include this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRender {
    Verbatim,
    Pruned,
    Omit,
}

/// The envelope. `payload` is typed for known kinds; use [`RawEnvelope`] to
/// read streams that may contain newer kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope<P> {
    pub schema_version: u32,
    pub event_id: EventId,
    /// Monotonic per-session sequence, allocated at commit (never by workers).
    pub seq: u64,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub device_id: DeviceId,
    /// Single-writer invariant: which authority epoch committed this.
    pub authority_epoch: u64,
    /// Fencing: which worker generation produced it (stale generations rejected).
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<EventId>,
    /// Milliseconds since epoch, assigned at commit.
    pub committed_at_ms: u64,
    pub render: RenderTargets,
    pub payload: P,
}

/// Envelope with the payload left as raw JSON — the forward-compat reader.
pub type RawEnvelope = EventEnvelope<serde_json::Value>;

/// Conservative over-approximation of one raw envelope's owned memory.
///
/// This is the common accounting unit for replay pages and live catch-up
/// buffers. It is deliberately independent of wire framing: the negotiated
/// frame limit governs encoded bytes, while this charge includes the fixed
/// envelope value, every owned ID string, and the payload's heap storage.
///
/// The exhaustive destructure is deliberate. Adding an envelope field must
/// fail this estimator at compile time until its weight is classified.
#[must_use]
pub fn envelope_weight_bytes(envelope: &RawEnvelope) -> usize {
    let EventEnvelope {
        schema_version: _,
        event_id,
        seq: _,
        session_id,
        branch_id,
        run_id,
        agent_id,
        device_id,
        authority_epoch: _,
        worker_generation: _,
        causation_id,
        correlation_id,
        committed_at_ms: _,
        render: _,
        payload,
    } = envelope;
    std::mem::size_of::<RawEnvelope>()
        .saturating_add(event_id.as_str().len())
        .saturating_add(session_id.as_str().len())
        .saturating_add(
            branch_id
                .as_ref()
                .map_or(0, |branch_id| branch_id.as_str().len()),
        )
        .saturating_add(run_id.as_ref().map_or(0, |run_id| run_id.as_str().len()))
        .saturating_add(
            agent_id
                .as_ref()
                .map_or(0, |agent_id| agent_id.as_str().len()),
        )
        .saturating_add(device_id.as_str().len())
        .saturating_add(
            causation_id
                .as_ref()
                .map_or(0, |causation_id| causation_id.as_str().len()),
        )
        .saturating_add(
            correlation_id
                .as_ref()
                .map_or(0, |correlation_id| correlation_id.as_str().len()),
        )
        .saturating_add(json_owned_heap_bytes(payload))
}

/// Conservative heap charge for `serde_json::Value`. The root `Value` itself
/// is already inside `RawEnvelope`; this counts allocations below it.
fn json_owned_heap_bytes(value: &serde_json::Value) -> usize {
    // `serde_json::Map` is a BTreeMap in this workspace. One sparse node holds
    // fixed-capacity key/value arrays and edges; 1 KiB per object plus 128
    // bytes per populated entry safely covers those nodes, String/Value
    // headers, and allocator bookkeeping without depending on serialized JSON
    // length (which badly undercounts arrays of primitives and sparse maps).
    const OBJECT_BASE_BYTES: usize = 1_024;
    const OBJECT_ENTRY_BYTES: usize = 128;
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => items.iter().map(json_owned_heap_bytes).fold(
            items
                .len()
                .saturating_mul(std::mem::size_of::<serde_json::Value>()),
            usize::saturating_add,
        ),
        serde_json::Value::Object(fields) if fields.is_empty() => 0,
        serde_json::Value::Object(fields) => fields.iter().fold(
            OBJECT_BASE_BYTES.saturating_add(fields.len().saturating_mul(OBJECT_ENTRY_BYTES)),
            |total, (key, value)| {
                total
                    .saturating_add(key.len())
                    .saturating_add(json_owned_heap_bytes(value))
            },
        ),
    }
}

impl<P> EventEnvelope<P> {
    /// Re-wrap with a different payload, preserving all envelope fields.
    pub fn map_payload<Q>(self, f: impl FnOnce(P) -> Q) -> EventEnvelope<Q> {
        EventEnvelope {
            schema_version: self.schema_version,
            event_id: self.event_id,
            seq: self.seq,
            session_id: self.session_id,
            branch_id: self.branch_id,
            run_id: self.run_id,
            agent_id: self.agent_id,
            device_id: self.device_id,
            authority_epoch: self.authority_epoch,
            worker_generation: self.worker_generation,
            causation_id: self.causation_id,
            correlation_id: self.correlation_id,
            committed_at_ms: self.committed_at_ms,
            render: self.render,
            payload: f(self.payload),
        }
    }
}
