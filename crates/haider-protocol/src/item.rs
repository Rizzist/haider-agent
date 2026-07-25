//! Turn items — the client-facing streaming lifecycle.
//!
//! Research-driven freeze decision (ADR-1): a uniform item lifecycle
//! (started → delta* → completed) over per-kind begin/end event pairs.
//! codex-rs shipped the flat taxonomy first and had to shim it away with a
//! 634-line legacy converter; we start where they ended. ACP's
//! `session/update` fan-out maps 1:1 onto these.

use crate::agent::ChildReport;
use crate::history::TodoItem;
use crate::ids::{AgentId, ArtifactRef, ItemId};
use serde::{Deserialize, Serialize};

/// One unit of turn content. `Extension` is the escape hatch for kinds this
/// schema version doesn't know (readers keep it raw).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum TurnItem {
    AgentMessage {
        text: String,
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
    },
    Extension {
        kind: String,
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
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
