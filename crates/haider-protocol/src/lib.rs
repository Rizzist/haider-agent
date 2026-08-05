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
pub mod context;
pub mod credential;
pub mod effect;
pub mod envelope;
pub mod error;
pub mod history;
pub mod hook;
pub mod ids;
pub mod item;
pub mod menu;
pub mod project_instructions;
pub mod provider;
pub mod rpc;
pub mod session;
pub mod state;
pub mod tool;
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
    // accounts
    Rotation(credential::RotationEvent),
    // token accounting
    Usage(provider::Usage),
}

/// Mid-turn input delivery (§3): steer is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    #[default]
    Steer,
    Queue,
}
