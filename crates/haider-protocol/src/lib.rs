//! haider-protocol — the frozen contracts of the Haider Code harness.
//!
//! One event stream, many encodings (Thesis 1): every surface — store, worker
//! IPC, RPC, TUI, GUI, sync — speaks THESE types. Serialized behavior is the
//! frozen artifact (golden fixtures in `tests/fixtures/`); Rust internals are
//! not. Schema changes require: version bump + ADR + old/new golden fixture +
//! upcaster where durable data is affected (BUILDGUIDE freeze rules).
//!
//! Forward compatibility: readers tolerate unknown payload kinds via
//! [`envelope::RawEnvelope`]; unknown JSON fields are ignored on read and
//! existing fields are never removed or re-typed within a schema version.

pub mod agent;
pub mod branch;
pub mod cache;
pub mod checkpoint;
pub mod computer;
pub mod context;
pub mod credential;
pub mod effect;
pub mod envelope;
pub mod error;
pub mod graph;
pub mod headless;
pub mod history;
pub mod hook;
pub mod ids;
pub mod image;
pub mod interaction;
pub mod item;
pub mod lockdown;
pub mod loom;
pub mod menu;
pub mod mobile;
pub mod permission;
pub mod pipe;
pub mod project_instructions;
pub mod provider;
pub mod queue;
pub mod retry;
pub mod rpc;
pub mod session;
pub mod session_fork;
pub mod state;
pub mod task;
pub mod tool;
pub mod typed_agent;
pub mod usage;
pub mod verify;

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-protocol";

/// The event payload union — every committed fact is one of these.
/// Readers encountering unknown kinds fall back to [`envelope::RawEnvelope`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    // lifecycle
    HarnessStatus(state::HarnessStatus),
    SessionState(state::SessionState),
    RunState(state::RunState),
    /// Durable, sanitized cause immediately preceding an errored run state.
    RunFailed {
        code: error::ErrorCode,
        message: String,
        retryable: bool,
        /// Safe structured presentation. Optional only so pre-E2 journals
        /// remain readable; every new failure producer populates it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<error::ErrorPresentation>,
    },
    /// A client-detected compatibility fault committed through the daemon so
    /// it survives reconnect/restart and is never reduced to silent omission.
    ClientDiagnostic {
        command_id: String,
        code: String,
        message: String,
    },
    IdleDecayed,
    // interaction
    MenuOpened(menu::Menu),
    MenuAnswered(menu::MenuAnswer),
    MenuClosed {
        menu: ids::MenuId,
        reason: menu::MenuCloseReason,
    },
    UserMessage {
        text: String,
        attachments: Vec<tool::AttachmentBlock>,
        /// Steer (deliver at next safe boundary) vs queue (hold to turn end).
        #[serde(default)]
        mode: DeliveryMode,
    },
    /// Volatile-looking queue control is journaled because its revision fence
    /// and watch delta must share the same serialized session truth.
    QueueChanged(queue::QueueDelta),
    /// Turn content flows as the ITEM lifecycle (started/delta/completed) —
    /// never a flat per-kind begin/end taxonomy (codex regret, ADR-1).
    Item(item::ItemEvent),
    // effects & tools
    Effect(effect::EffectPhase),
    ToolResult {
        call_id: String,
        result: tool::BoundedResult,
    },
    // history
    NodeCommitted(history::TreeNode),
    // agents
    AgentSpawned(agent::AgentManifest),
    AgentReport(agent::ChildReport),
    AgentChipState {
        agent: ids::AgentId,
        chip: agent::ChipState,
    },
    // verification
    GateReport(verify::GateReport),
    // convergence graph (daemon authority; M2a adds trusted process signals)
    GraphPinned(graph::GraphPinned),
    GraphAttemptOpened(graph::GraphAttemptOpened),
    EvidenceRecorded(graph::EvidenceRecorded),
    GraphGateSatisfied(graph::GraphGateSatisfied),
    GraphAdvanced(graph::GraphAdvanced),
    GraphNodeReadied(graph::GraphNodeReadied),
    GraphBlocked(graph::GraphBlocked),
    GraphCompleted(graph::GraphCompleted),
    GraphAbandoned(graph::GraphAbandoned),
    GraphSuperseded(graph::GraphSuperseded),
    GraphFinalizationDeferred(graph::GraphFinalizationDeferred),
    ProcessSignalRecorded(graph::ProcessSignalRecorded),
    GraphRunSetOpened(graph::GraphRunSetOpened),
    TodoGraphAttached(graph::TodoGraphAttached),
    ChildGraphAttached(graph::ChildGraphAttached),
    ChildTemplateObserved(graph::ChildTemplateObserved),
    ChildTemplatePromoted(graph::ChildTemplatePromoted),
    // accounts
    Rotation(credential::RotationEvent),
    // token accounting
    Usage(provider::Usage),
    // append-only workspace mutation history
    CheckpointRecorded(checkpoint::CheckpointRecorded),
    /// Raw daemon security facts. Prompt compaction may summarize these, but
    /// durable/native-pipe records retain the complete payload.
    #[serde(rename = "lockdown.refused")]
    LockdownRefused(lockdown::LockdownRefused),
    #[serde(rename = "lockdown.quota")]
    LockdownQuota(lockdown::LockdownQuota),
    #[serde(rename = "provider.trust_changed")]
    ProviderTrustChanged(lockdown::ProviderTrustChanged),
}

/// Mid-turn input delivery (§3): steer is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    #[default]
    Steer,
    Queue,
    Subturn,
}
