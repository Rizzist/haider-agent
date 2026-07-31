//! The history tree (§6): single-parent semantic nodes; branches are named
//! refs; compaction is a first-class immutable node; nothing silently discarded.

use crate::agent::ChildReport;
use crate::ids::{AgentId, ArtifactRef, NodeId};
use crate::provider::FinishReason;
use crate::tool::AttachmentBlock;
use crate::verify::VerifyVerdict;
use serde::{Deserialize, Serialize};

/// Typed extension item carrying [`CompactionIntent`] before summarization.
pub const COMPACTION_INTENT_EXTENSION_KIND: &str = "context_compaction_intent_v1";
/// Typed extension item marking a same-run MaxTokens continuation seam.
pub const CONTINUATION_CHECKPOINT_EXTENSION_KIND: &str = "continuation_checkpoint_v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub node: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeKind {
    UserTurn {
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
    },
    AssistantCommit {
        text: String,
        /// The turn's verification verdict — never an absence (§9.2).
        verdict: VerifyVerdict,
    },
    ToolExchange {
        tool: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<ArtifactRef>,
    },
    ChildSpawn {
        agent: AgentId,
    },
    ChildResult {
        report: ChildReport,
    },
    /// Compaction: covered range stays navigable; you can fork from before it.
    Compaction {
        covers_from: NodeId,
        covers_to: NodeId,
        summary_artifact: ArtifactRef,
        tokens_before: u64,
        tokens_after: u64,
        resume_cause: CompactionResume,
    },
    /// Cross-branch reuse is explicit — the ancestry stays a tree (§6).
    ResultImport {
        from_node: NodeId,
        summary: String,
    },
    /// Non-prompt metadata: auto-title, blurb, head callsign, etc. (§6).
    Annotation {
        annotation: AnnotationKind,
        data: serde_json::Value,
    },
    /// The live plan (§10). Completion unpins it into history as this node.
    Todos {
        items: Vec<TodoItem>,
    },
}

/// Why a compaction ended and what resumed after it (mid-turn auto-continue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionResume {
    /// Fired mid-turn; the parked turn auto-resumed (never a stopping point).
    AutoMidTurn,
    /// Explicit /compact from idle — ends at idle.
    ManualIdle,
}

/// Durable plan written before private summarization starts. The final
/// [`NodeKind::Compaction`] is the atomic projection switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionIntent {
    pub operation_id: String,
    pub covers_from: NodeId,
    pub covers_to: NodeId,
    pub resume_cause: CompactionResume,
}

/// Durable prompt-omitted seam for one same-run output continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationCheckpoint {
    pub reason: FinishReason,
    pub request_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    AutoTitle,
    Blurb,
    HeadCallsign,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u32,
    pub text: String,
    pub state: TodoState,
    /// `id` of the item this one depends on (blocked until it completes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dep: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    Listed,
    Processing,
    Completed,
}
