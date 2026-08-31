//! Turn items — the client-facing streaming lifecycle.
//!
//! Research-driven freeze decision (ADR-1): a uniform item lifecycle
//! (started → delta* → completed) over per-kind begin/end event pairs.
//! codex-rs shipped the flat taxonomy first and had to shim it away with a
//! 634-line legacy converter; we start where they ended. ACP's
//! `session/update` fan-out maps 1:1 onto these.

use crate::agent::ChildReport;
use crate::error::ErrorPresentation;
use crate::history::TodoItem;
use crate::ids::{AgentId, ArtifactRef, ItemId};
use serde::{Deserialize, Serialize};

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Durable extension kind linking a command item to direct composer origin.
///
/// This deliberately uses the existing [`TurnItem::Extension`] escape hatch
/// instead of widening `CommandExecution`: older clients keep decoding and
/// rendering the first-class command item byte-for-byte, while prompt/audit
/// consumers can distinguish a user `!` command from model-initiated exec.
pub const USER_COMMAND_ORIGIN_EXTENSION_KIND: &str = "user_command_origin_v1";

/// Origin values carried by [`UserCommandOriginV1`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionOrigin {
    UserCommand,
}

/// Hidden durable marker for one user-initiated [`TurnItem::CommandExecution`].
///
/// The marker's envelope is durable but not UI-rendered. `command_item_id`
/// binds provenance to the visible item, and `call_id` provides an independent
/// receipt-coordinate check during prompt reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCommandOriginV1 {
    pub origin: CommandExecutionOrigin,
    pub command_item_id: ItemId,
    pub call_id: String,
}

impl UserCommandOriginV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: USER_COMMAND_ORIGIN_EXTENSION_KIND.into(),
            data: serde_json::to_value(self)?,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        Self::try_from_extension_item(item).ok().flatten()
    }

    /// Strict parser for durable prompt/audit consumers. Unknown extension
    /// kinds are not this marker; a malformed known marker is an error.
    pub fn try_from_extension_item(item: &TurnItem) -> Result<Option<Self>, serde_json::Error> {
        let TurnItem::Extension { kind, data } = item else {
            return Ok(None);
        };
        if kind != USER_COMMAND_ORIGIN_EXTENSION_KIND {
            return Ok(None);
        }
        serde_json::from_value(data.clone()).map(Some)
    }
}

/// One unit of turn content. `Extension` is the escape hatch for kinds this
/// schema version doesn't know (readers keep it raw).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum TurnItem {
    AgentMessage {
        text: String,
    },
    /// Assistant text whose provider stream ended after content committed.
    /// This variant is deliberately not replayed as completed assistant
    /// history; an explicit recovery action decides whether to prime a new
    /// turn from it or retry fresh.
    IncompleteAgentMessage {
        text: String,
        interruption: ErrorPresentation,
    },
    Reasoning {
        summary: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
        status: ToolStatus,
    },
    CommandExecution {
        call_id: String,
        command: String,
        status: ToolStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    FileChange {
        path: String,
        added: u32,
        removed: u32,
    },
    ChildSpawn {
        agent: AgentId,
    },
    ChildResult {
        report: ChildReport,
    },
    Plan {
        items: Vec<TodoItem>,
    },
    ContextCompaction {
        summary_artifact: ArtifactRef,
        /// Context footprint before compaction, in tokens. ADDITIVE +
        /// OPTIONAL (TUI3b): the numbers are already protocol-blessed on
        /// `NodeKind::Compaction`; carrying them on the item lets clients
        /// render `⊟ compacted 118k → 12k` without tree access.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        /// Context footprint after compaction, in tokens (see above).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<u64>,
        /// True when the counts use a documented local estimate rather than a
        /// model tokenizer or provider-reported request-local usage.
        #[serde(default, skip_serializing_if = "is_false")]
        tokens_estimated: bool,
    },
    Extension {
        kind: String,
        data: serde_json::Value,
    },
    /// A provider refusal is visible and durable, but remains semantically
    /// distinct from assistant text and is never replayed as an answer.
    Refusal {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    /// Turn was cancelled while the tool was open — an outcome, never a
    /// failure (frozen law; additive variant, ADR-2).
    Cancelled,
    /// The call was understood but refused before an effect ran.
    Rejected,
    /// The call could not apply against the observed workspace state.
    Conflict,
    /// Dispatch crossed an ambiguity boundary and the outcome is not known.
    Unknown,
}

/// The lifecycle events. Deltas are keyed by `item_id`; `Completed` carries
/// the final item (replace semantics — clients replace, never append-merge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ItemEvent {
    Started { item_id: ItemId, item: TurnItem },
    Delta { item_id: ItemId, delta: ItemDelta },
    Completed { item_id: ItemId, item: TurnItem },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "delta", rename_all = "snake_case")]
pub enum ItemDelta {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolArgs {
        fragment: String,
    },
    /// Process output is BYTES (base64) — command output may be invalid UTF-8
    /// (codex lesson: string chunks corrupt real-world exec output).
    CommandOutput {
        stream: OutputStream,
        chunk_b64: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}
