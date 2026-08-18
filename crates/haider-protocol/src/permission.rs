//! Interactive operating-system permission recovery.
//!
//! These values are durable UI facts, not Haider authorization credentials.
//! A [`PermissionGrantNeeded`] event says that an already-authorized computer
//! effect is parked at the operating-system gate and tells a client exactly
//! which controls it may render. It never bypasses the OS permission check.

use crate::ids::{EffectId, MenuId};
use serde::{Deserialize, Serialize};

/// Additive event family. It deliberately lives beside the frozen
/// [`crate::EventPayload`] union: older clients decode these envelopes as an
/// unknown payload kind and continue replay, while permission-aware clients
/// opt in through [`Self::from_payload_value`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionEventPayload {
    PermissionGrantNeeded(PermissionGrantNeeded),
    PermissionGrantResolved(PermissionGrantResolved),
}

impl PermissionEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    pub fn from_payload_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

/// macOS TCC permission needed by a native computer action.
///
/// The enum is intentionally platform-neutral on the wire so non-macOS
/// clients can replay a macOS-authored session without special decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPermission {
    ScreenRecording,
    Accessibility,
}

impl SystemPermission {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScreenRecording => "screen_recording",
            Self::Accessibility => "accessibility",
        }
    }
}

/// Server-enumerated controls for an in-session grant card.
///
/// `OpenSettings` uses [`PermissionGrantNeeded::settings_url`]. `Retry`
/// asks the daemon to recheck immediately in addition to its automatic poll.
/// `RestartDaemon` is a fallback only when the event says a restart is
/// required or pending; clients should reconnect and retry the run after the
/// daemon's ordinary graceful drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantAction {
    OpenSettings,
    Retry,
    RestartDaemon,
}

/// Durable, prompt-omitted signal that an authorized computer action is
/// parked at a real operating-system permission boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrantNeeded {
    /// Stable correlation for button actions and the matching resolution.
    pub request_id: String,
    /// Existing menu-answer coordinates used by the Retry button. The raw
    /// event is emitted only after this MenuOpened fact is durable.
    pub menu_id: MenuId,
    pub request_seq: u64,
    pub opening_generation: u64,
    pub call_id: String,
    pub effect_id: EffectId,
    pub permission: SystemPermission,
    /// Human pane name, for example
    /// `System Settings > Privacy & Security > Screen Recording`.
    pub pane_name: String,
    /// Exact allow-listed System Settings deep link.
    pub settings_url: String,
    #[serde(default)]
    pub actions: Vec<PermissionGrantAction>,
    /// True only after the daemon has established that the live process
    /// cannot consume the newly granted permission and is arranging/falling
    /// back to a restart.
    #[serde(default)]
    pub auto_restart_pending: bool,
    /// Bounded automatic polling window for this parked action.
    pub poll_timeout_ms: u64,
}

/// Why a grant card can be dismissed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantResolution {
    Granted,
    TimedOut,
    RestartRequired,
    Cancelled,
}

/// Durable closure for one [`PermissionGrantNeeded`] card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrantResolved {
    pub request_id: String,
    pub permission: SystemPermission,
    pub resolution: PermissionGrantResolution,
    /// True means the parked computer action will be executed again inside
    /// the same turn without a new model tool call.
    pub retrying_parked_action: bool,
}
