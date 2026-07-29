//! Provider-agnostic model IR (§4): canonical blocks and stream events.
//! Normalize semantics, not quirks; preserve provider-native state as opaque
//! envelopes; capability documents make degradation explicit, never silent.

use crate::ids::CredentialAlias;
use crate::tool::AttachmentBlock;
use serde::{Deserialize, Serialize};

/// Stable durable-extension kind used for provider-native continuation state.
pub const PROVIDER_OPAQUE_EXTENSION_KIND: &str = "provider_opaque";

/// Canonical message content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Reasoning {
        summary: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        preview: String,
        truncated: bool,
    },
    Attachment(AttachmentBlock),
    /// Provider-native continuation state, opaque and provider-keyed —
    /// an Anthropic session continues losslessly on Anthropic (§4).
    ProviderOpaque {
        provider: String,
        data: serde_json::Value,
    },
}

/// Canonical streaming events, normalized across adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Provider refusal content remains semantically distinct from assistant
    /// text. Consumers may display it, but must not replay it as an answer.
    RefusalDelta {
        text: String,
    },
    /// Provider-native continuation state for lossless, same-family replay.
    ProviderOpaque {
        provider: String,
        data: serde_json::Value,
    },
    ToolCallStart {
        call_id: String,
        name: String,
    },
    ToolCallArgsDelta {
        call_id: String,
        args_fragment: String,
    },
    ToolCallEnd {
        call_id: String,
    },
    UsageUpdate(Usage),
    Finish {
        reason: FinishReason,
    },
}

/// Cancellation is an OUTCOME, never an error (ACP contract rule): an aborted
/// turn finishes with `Cancelled`, and surfaces must not render it as failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Cancelled,
    Error,
}

/// Multidimensional, honest token accounting (§4): tagged by source quality
/// and by account so tokenomics never blends subscription with metered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cached: u64,
    pub source: UsageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<CredentialAlias>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    ProviderReported,
    LocallyExact,
    Estimated,
}

/// What an adapter supports — requested features resolve explicitly (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDoc {
    pub provider: String,
    pub parallel_tools: FeatureResolve,
    pub streaming_tool_args: FeatureResolve,
    pub vision: FeatureResolve,
    pub thinking_visible: FeatureResolve,
    pub context_limit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureResolve {
    Native,
    ExplicitlyEmulated,
    Unsupported,
}
