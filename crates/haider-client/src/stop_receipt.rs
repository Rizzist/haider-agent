//! Generation-bound completion receipt for an externally stopped daemon.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DAEMON_STOP_COMPLETION_SCHEMA: &str = "haider.daemon-stop-completion.v1";
/// Handshake name that registers interest in one exact shutdown completion.
pub const DAEMON_STOP_CLIENT_NAME: &str = "haider-daemon-stop";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStopCompletion {
    Graceful,
    Forced,
    Failed,
}

/// Small, secret-free bridge between the closed IPC lifecycle and process
/// exit. The random instance identity plus durable generation prevent a stale
/// receipt from confirming a different daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStopReceipt {
    pub schema: String,
    pub instance_id: String,
    pub generation: u64,
    pub pid: u32,
    pub completion: DaemonStopCompletion,
}

/// Returns the unique receipt path for one authenticated daemon generation.
/// Wire-provided instance IDs are rejected unless they have the daemon's exact
/// 128-bit lowercase-hex shape, so they can never influence path structure.
#[must_use]
pub fn daemon_stop_receipt_path(
    store_dir: &Path,
    generation: u64,
    instance_id: &str,
) -> Option<PathBuf> {
    if instance_id.len() != 32
        || !instance_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    Some(store_dir.join(format!(".daemon-stop-{generation}-{instance_id}.json")))
}
