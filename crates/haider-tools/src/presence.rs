//! Computer-use presence: the human-visible "Haider is controlling …"
//! indicator and its Stop control.
//!
//! This module is platform neutral. It owns
//!
//! * the session-scoped state machine ([`PresenceMachine`]) that decides when
//!   an indicator appears (first CU action of a run), updates (every action),
//!   and disappears (run end, cancellation, Stop, or
//!   [`CU_PRESENCE_IDLE_SECS`] of inactivity);
//! * the line-delimited JSON wire vocabulary ([`PresenceCommand`] /
//!   [`PresenceEvent`]) spoken between the daemon and a rendering surface
//!   (the desktop overlay helper, and by contract the browser adapter);
//! * the rasterised overlay art shared by every desktop renderer; and
//! * the desktop overlay helper entry point, which `haiderd` runs as a
//!   separate process (`haiderd --cu-presence-overlay`) so the UI owns a real
//!   main thread and never shares one with the daemon's async runtime.
//!
//! See `docs/cu-presence.md` for the end-to-end design.

#[path = "presence/art.rs"]
pub mod art;

#[cfg(target_os = "macos")]
#[path = "presence/overlay_macos.rs"]
mod overlay_macos;

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
#[path = "presence/overlay_windows.rs"]
mod overlay_windows;

#[cfg(target_os = "linux")]
#[path = "presence/overlay_linux.rs"]
mod overlay_linux;

use haider_protocol::computer::{CU_PRESENCE_IDLE_SECS, ComputerAction};
use haider_protocol::mobile::MobileAction;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Hidden `haiderd` argument that runs the desktop overlay helper instead of
/// the daemon. Reusing the shipped daemon executable means no new release
/// member, installer entry or code-signing identity is needed.
pub const PRESENCE_HELPER_ARG: &str = "--cu-presence-overlay";

/// Absolute path of an alternative overlay helper executable (tests, or a
/// packaging layout where the daemon is not named `haiderd`).
pub const PRESENCE_HELPER_ENV: &str = "HAIDER_CU_PRESENCE_HELPER";

/// `0`/`off`/`false` disables the desktop overlay (headless CI, kiosk
/// sessions). Stop routing and the TUI chip remain active.
pub const PRESENCE_DISABLE_ENV: &str = "HAIDER_CU_PRESENCE";

/// Evidence-only (macOS): `1` makes the overlay capturable so documentation
/// screenshots can show it. Model-facing captures then include it too.
pub const PRESENCE_EVIDENCE_CAPTURABLE_ENV: &str = "HAIDER_CU_PRESENCE_EVIDENCE_CAPTURABLE";

/// Value written into every input event Haider synthesises (macOS
/// `kCGEventSourceUserData`, Windows `dwExtraInfo`). Overlay helpers ignore
/// a Stop click carrying it, so the agent can never press its own Stop.
pub const SYNTHETIC_INPUT_TAG: i64 = 0x4841_4944; // "HAID"

/// Default idle window after which an unused indicator retires.
pub const PRESENCE_IDLE_TIMEOUT: Duration = Duration::from_secs(CU_PRESENCE_IDLE_SECS);

/// Stopped leases that were never ended (a leaked dispatcher) are reclaimed
/// after this long so the machine cannot grow without bound.
pub const STOPPED_LEASE_RETENTION: Duration = Duration::from_secs(600);

/// Upper bound the daemon waits for an overlay to acknowledge a pointer
/// command before the input is posted (the overlay moves its Stop badge out
/// of the way of a synthetic click). Missing acks never block control.
pub const POINTER_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// What is being controlled. Each surface renders its own indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceSurface {
    /// The local desktop driven by the `computer` tool.
    Screen,
    /// The Android device driven by the `mobile` tool.
    Phone,
    /// A browser attached for automation (webextract-2 contract).
    Browser,
}

impl PresenceSurface {
    /// The badge / chip text shown while presence is active.
    #[must_use]
    pub const fn badge_label(self) -> &'static str {
        match self {
            Self::Screen => "Haider is controlling this screen",
            Self::Phone => "Haider is controlling your phone",
            Self::Browser => "Haider is controlling this browser",
        }
    }

    /// Short noun used by the TUI header chip.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Phone => "phone",
            Self::Browser => "browser",
        }
    }
}

/// How the agent pointer marks one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceMark {
    /// Observation only (screenshot, cursor query, accessibility inspect).
    Observe,
    Move,
    Click,
    DoubleClick,
    RightClick,
    Press,
    Release,
    Drag,
    Scroll,
    Type,
    Key,
    Wait,
}

impl PresenceMark {
    /// The mark for one desktop `computer` action.
    #[must_use]
    pub const fn for_computer_action(action: &ComputerAction) -> Self {
        match action {
            ComputerAction::Screenshot
            | ComputerAction::CursorPosition
            | ComputerAction::Inspect { .. } => Self::Observe,
            ComputerAction::LeftClick { .. } | ComputerAction::MiddleClick => Self::Click,
            ComputerAction::DoubleClick | ComputerAction::TripleClick => Self::DoubleClick,
            ComputerAction::RightClick => Self::RightClick,
            ComputerAction::LeftMouseDown => Self::Press,
            ComputerAction::LeftMouseUp => Self::Release,
            ComputerAction::MouseMove { .. } => Self::Move,
            ComputerAction::LeftClickDrag { .. } => Self::Drag,
            ComputerAction::Type { .. } => Self::Type,
            ComputerAction::Key { .. } => Self::Key,
            ComputerAction::Scroll { .. } => Self::Scroll,
            ComputerAction::Wait { .. } => Self::Wait,
        }
    }

    /// The mark for one `mobile` action, or `None` when the action does not
    /// operate the phone's screen (SMS reads, app listing) and therefore
    /// must not raise a "controlling your phone" indicator.
    #[must_use]
    pub const fn for_mobile_action(action: &MobileAction) -> Option<Self> {
        match action {
            MobileAction::Screenshot {}
            | MobileAction::A11yTree {}
            | MobileAction::Inspect { .. } => Some(Self::Observe),
            MobileAction::Tap { .. } => Some(Self::Click),
            MobileAction::LongPress { .. } => Some(Self::Press),
            MobileAction::Swipe { .. } => Some(Self::Drag),
            MobileAction::Type { .. } => Some(Self::Type),
            MobileAction::Key { .. } | MobileAction::OpenApp { .. } => Some(Self::Key),
            MobileAction::ListApps {} | MobileAction::SmsRead { .. } => None,
        }
    }

    /// Whether the overlay should move its clickable badge away from the
    /// pointer before this action posts input.
    #[must_use]
    pub const fn posts_pointer_input(self) -> bool {
        matches!(
            self,
            Self::Click
                | Self::DoubleClick
                | Self::RightClick
                | Self::Press
                | Self::Release
                | Self::Drag
                | Self::Scroll
        )
    }

    /// Short caption drawn beside the agent pointer.
    #[must_use]
    pub const fn caption(self) -> &'static str {
        match self {
            Self::Observe => "Haider · looking",
            Self::Move => "Haider",
            Self::Click | Self::DoubleClick | Self::RightClick | Self::Press | Self::Release => {
                "Haider · click"
            }
            Self::Drag => "Haider · drag",
            Self::Scroll => "Haider · scroll",
            Self::Type => "Haider · typing",
            Self::Key => "Haider · keys",
            Self::Wait => "Haider · waiting",
        }
    }
}

/// A point in global desktop coordinates: logical points with a top-left
/// origin at the primary display (Quartz global space on macOS, virtual
/// screen pixels on Windows, root-window pixels on X11).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PresencePoint {
    pub x: f64,
    pub y: f64,
}

/// Why a presence lease ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceEndReason {
    RunEnded,
    Cancelled,
    Idle,
    Stopped,
}

/// Daemon → surface command, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresenceCommand {
    /// Make the indicator visible with its badge label.
    Show {
        surface: PresenceSurface,
        label: String,
    },
    /// Animate the agent pointer to `point` (or mark in place when absent)
    /// and flash `mark`. Surfaces answer [`PresenceEvent::Ack`] with `seq`
    /// once any clickable chrome is clear of the target.
    Pointer {
        surface: PresenceSurface,
        seq: u64,
        mark: PresenceMark,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        point: Option<PresencePoint>,
    },
    /// Stop was accepted; show "Stopping…" until the following hide.
    Stopping { surface: PresenceSurface },
    /// Retire the indicator.
    Hide {
        surface: PresenceSurface,
        reason: PresenceEndReason,
    },
}

impl PresenceCommand {
    #[must_use]
    pub const fn surface(&self) -> PresenceSurface {
        match self {
            Self::Show { surface, .. }
            | Self::Pointer { surface, .. }
            | Self::Stopping { surface }
            | Self::Hide { surface, .. } => *surface,
        }
    }
}

/// Surface → daemon event, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresenceEvent {
    /// The surface is up. `capture_excluded` states whether its windows are
    /// excluded from the backend's screen capture by construction.
    Ready {
        platform: String,
        capture_excluded: bool,
    },
    Ack {
        seq: u64,
    },
    /// The human pressed Stop.
    Stop,
    /// A non-fatal rendering problem worth logging.
    Error {
        message: String,
    },
}

/// Why an action was refused by the presence layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceRefusal {
    /// The human pressed Stop for this run; no further CU action may start.
    Stopped,
}

#[derive(Debug, Clone)]
struct Lease {
    surface: PresenceSurface,
    last_activity: Instant,
    in_flight: u32,
    stopped: bool,
}

/// Session-scoped presence state. `K` identifies one run (the daemon uses
/// `session/run`). The machine is pure: callers pass `now` and forward the
/// returned commands to the matching surface renderer.
#[derive(Debug)]
pub struct PresenceMachine<K: Ord + Clone> {
    leases: BTreeMap<K, Lease>,
    shown: BTreeSet<PresenceSurface>,
    idle_timeout: Duration,
    next_seq: u64,
}

impl<K: Ord + Clone> Default for PresenceMachine<K> {
    fn default() -> Self {
        Self::new(PRESENCE_IDLE_TIMEOUT)
    }
}

impl<K: Ord + Clone> PresenceMachine<K> {
    #[must_use]
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            leases: BTreeMap::new(),
            shown: BTreeSet::new(),
            idle_timeout,
            next_seq: 1,
        }
    }

    /// Records the start of one CU action. Returns the commands to render,
    /// or [`PresenceRefusal::Stopped`] when the human already stopped this
    /// run — the caller must then cancel the action without executing it.
    pub fn begin_action(
        &mut self,
        key: K,
        surface: PresenceSurface,
        mark: PresenceMark,
        point: Option<PresencePoint>,
        now: Instant,
    ) -> Result<(u64, Vec<PresenceCommand>), PresenceRefusal> {
        if let Some(lease) = self.leases.get(&key)
            && lease.stopped
        {
            return Err(PresenceRefusal::Stopped);
        }
        let lease = self.leases.entry(key).or_insert(Lease {
            surface,
            last_activity: now,
            in_flight: 0,
            stopped: false,
        });
        lease.surface = surface;
        lease.last_activity = now;
        lease.in_flight = lease.in_flight.saturating_add(1);
        let mut commands = Vec::with_capacity(2);
        if self.shown.insert(surface) {
            commands.push(PresenceCommand::Show {
                surface,
                label: surface.badge_label().to_owned(),
            });
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        commands.push(PresenceCommand::Pointer {
            surface,
            seq,
            mark,
            point,
        });
        Ok((seq, commands))
    }

    /// Records that one action finished; the idle window starts now.
    pub fn finish_action(&mut self, key: &K, now: Instant) {
        if let Some(lease) = self.leases.get_mut(key) {
            lease.in_flight = lease.in_flight.saturating_sub(1);
            lease.last_activity = now;
        }
    }

    /// The run ended (normally or cancelled). Drops the lease and hides the
    /// surface once no active lease still needs it.
    pub fn end(&mut self, key: &K, reason: PresenceEndReason) -> Vec<PresenceCommand> {
        let Some(lease) = self.leases.remove(key) else {
            return Vec::new();
        };
        self.hide_if_unused(lease.surface, reason)
    }

    /// Expires idle leases and reclaims stale stopped ones.
    pub fn tick(&mut self, now: Instant) -> Vec<PresenceCommand> {
        let idle_timeout = self.idle_timeout;
        let mut expired_surfaces = BTreeSet::new();
        self.leases.retain(|_, lease| {
            let quiet = now.saturating_duration_since(lease.last_activity);
            if lease.stopped {
                return quiet < STOPPED_LEASE_RETENTION;
            }
            if lease.in_flight == 0 && quiet >= idle_timeout {
                expired_surfaces.insert(lease.surface);
                return false;
            }
            true
        });
        expired_surfaces
            .into_iter()
            .flat_map(|surface| self.hide_if_unused(surface, PresenceEndReason::Idle))
            .collect()
    }

    /// The human pressed Stop on `surface`. Every active lease on it is
    /// marked stopped (further actions are refused) and returned so the
    /// caller can cancel the in-flight action and the run.
    pub fn stop(&mut self, surface: PresenceSurface) -> (Vec<K>, Vec<PresenceCommand>) {
        let mut stopped = Vec::new();
        for (key, lease) in &mut self.leases {
            if lease.surface == surface && !lease.stopped {
                lease.stopped = true;
                stopped.push(key.clone());
            }
        }
        let mut commands = Vec::new();
        if self.shown.remove(&surface) {
            commands.push(PresenceCommand::Stopping { surface });
            commands.push(PresenceCommand::Hide {
                surface,
                reason: PresenceEndReason::Stopped,
            });
        }
        (stopped, commands)
    }

    #[must_use]
    pub fn is_shown(&self, surface: PresenceSurface) -> bool {
        self.shown.contains(&surface)
    }

    #[must_use]
    pub fn is_stopped(&self, key: &K) -> bool {
        self.leases.get(key).is_some_and(|lease| lease.stopped)
    }

    /// Whether any lease (active or stopped) remains, i.e. whether a caller
    /// still needs to call [`Self::tick`].
    #[must_use]
    pub fn has_leases(&self) -> bool {
        !self.leases.is_empty()
    }

    fn hide_if_unused(
        &mut self,
        surface: PresenceSurface,
        reason: PresenceEndReason,
    ) -> Vec<PresenceCommand> {
        let still_used = self
            .leases
            .values()
            .any(|lease| lease.surface == surface && !lease.stopped);
        if !still_used && self.shown.remove(&surface) {
            vec![PresenceCommand::Hide { surface, reason }]
        } else {
            Vec::new()
        }
    }
}

/// How the daemon should launch the desktop overlay helper, or `None` when
/// the overlay is disabled or no helper executable is known (for example a
/// test binary embedding the daemon).
#[must_use]
pub fn presence_helper_command() -> Option<(PathBuf, Vec<String>)> {
    if !cfg!(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux"
    )) {
        return None;
    }
    if std::env::var(PRESENCE_DISABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    }) {
        return None;
    }
    if let Some(explicit) = std::env::var_os(PRESENCE_HELPER_ENV) {
        let path = PathBuf::from(explicit);
        return path.is_absolute().then_some((path, Vec::new()));
    }
    let current = std::env::current_exe().ok()?;
    let stem = current.file_stem()?.to_str()?;
    (stem == "haiderd").then(|| (current, vec![PRESENCE_HELPER_ARG.to_owned()]))
}

/// Runs the desktop overlay helper on the calling (main) thread until its
/// stdin closes. Returns the process exit status.
#[must_use]
pub fn run_presence_overlay_helper() -> i32 {
    #[cfg(target_os = "macos")]
    {
        overlay_macos::run()
    }
    #[cfg(target_os = "windows")]
    {
        overlay_windows::run()
    }
    #[cfg(target_os = "linux")]
    {
        overlay_linux::run()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        eprintln!("haiderd: the computer-use presence overlay is unavailable on this platform");
        69
    }
}

/// Parses one helper stdin line. Blank lines are ignored; malformed lines
/// are reported but never fatal, so a newer daemon cannot crash an older
/// helper.
pub fn parse_command_line(line: &str) -> Option<Result<PresenceCommand, String>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(serde_json::from_str(trimmed).map_err(|error| error.to_string()))
}

/// Encodes one helper stdout line (without the trailing newline).
#[must_use]
pub fn encode_event(event: &PresenceEvent) -> String {
    serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"event":"error","message":"encode"}"#.into())
}

#[cfg(test)]
#[path = "presence_tests.rs"]
mod tests;
