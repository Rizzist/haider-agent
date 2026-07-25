//! Client API framing (§7): the frontend parity contract. TUI, GUI, and
//! headless controllers speak the SAME surface — no privileged paths.
//! Subscriptions resume from a durable cursor; reconnection never loses events.
//!
//! Wire framing (frozen): strict JSONL — LF (0x0A) is the ONLY frame
//! delimiter; one trailing CR is stripped; U+2028/U+2029 are legal inside JSON
//! strings and MUST NOT split frames (generic line readers are non-compliant).
//!
//! Command semantics: a successful response means ACCEPTED/queued; failures
//! after acceptance flow through events, never a second response for the same
//! request. Mutating methods accept a client-minted `command_id` for
//! idempotent retry.

use crate::error::HaiderError;
use serde::{Deserialize, Serialize};

/// Current RPC protocol version (negotiated at hello).
pub const RPC_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    /// Oldest version the client can still speak (N−1 compatibility).
    pub min_supported: u32,
    pub client_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_version: u32,
    pub server_version: String,
}

/// Request envelope. `method` is namespaced ("session.list", "menu.answer",
/// "run.submit", "events.subscribe", …); params are method-defined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    /// Client-minted idempotency key for mutating methods — a retried command
    /// with the same key is acknowledged, never re-executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: RpcOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcOutcome {
    Result(serde_json::Value),
    Error(HaiderError),
}

/// Subscribe to a session's event stream from a cursor. The server replays
/// committed envelopes with seq > since_seq, then streams live; a slow
/// consumer is dropped with a `Lagged` notice rather than wedging the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeParams {
    pub session: crate::ids::SessionId,
    pub since_seq: u64,
}

/// Non-request server→client notifications on a subscription channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "notice", rename_all = "snake_case")]
pub enum SubscriptionNotice {
    /// Catch-up replay is complete; everything after this is live (t3code
    /// pattern — clients need an explicit synchronized marker).
    Synchronized { last_seq: u64 },
    /// Consumer fell too far behind; resubscribe from last_seq.
    Lagged { last_seq: u64 },
    /// Subscription's session was closed/deleted.
    Ended,
}
