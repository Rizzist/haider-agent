//! Provider-lockdown facts. Payloads are self-sufficient so native Pipe and
//! other ADE clients never need to reconstruct security state from prose.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockdownRefused {
    pub provider: String,
    pub tool: String,
    pub reason: String,
    pub tools_allowed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockdownQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub used: u64,
    pub limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTrustChanged {
    pub provider: String,
    pub previous: String,
    pub trust: String,
    pub revision: u64,
}
