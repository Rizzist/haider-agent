//! W-C M2 — desktop notifications via OSC 9 (zero dependency).
//!
//! An OSC 9 sequence (`ESC ] 9 ; <text> BEL`) is a terminal-native desktop
//! notification honored by iTerm2, kitty, WezTerm, and others. The TUI emits
//! it through the terminal it already owns (stdout); `haider run` emits it to
//! stderr's tty. Both go through THIS module so the text is bounded, MASKED
//! (P1, one authority — `format::mask_identity`), and can never carry a stray
//! control byte that would break the escape sequence or leak into a pipe.
//!
//! Notifications fire only on a turn reaching a TERMINAL state (Done / Errored)
//! or an attention PARK (PermissionRequired / InputRequired / a device wait) —
//! never mid-stream. The focus gate and the on/off toggle live with the caller.

use crate::format::mask_identity;
use haider_protocol::state::{RunState, WaitReason};

/// A hard cap on the notification text (defense against a runaway title).
pub const MAX_NOTIFICATION_LEN: usize = 120;

/// The attention a run state warrants, if any. Everything not listed here —
/// every mid-stream state (Thinking / Streaming / RunningTool / …) — yields
/// `None`, so a notification NEVER fires on a stream chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// A turn completed naturally.
    Done,
    /// A turn ended in error.
    Errored,
    /// Parked on a permission menu.
    Permission,
    /// Parked on an input/question menu.
    Input,
    /// Parked waiting on an unreachable device.
    WaitingDevice,
}

impl Attention {
    /// Whether this is a terminal completion (vs an attention park) — the
    /// caller uses it only for message wording; both fire a notification.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Errored)
    }
}

/// Map a run state to the attention it warrants (`None` = do not notify).
#[must_use]
pub fn attention_for(state: &RunState) -> Option<Attention> {
    match state {
        RunState::Done => Some(Attention::Done),
        RunState::Errored => Some(Attention::Errored),
        RunState::PermissionRequired { .. } => Some(Attention::Permission),
        RunState::InputRequired { .. } => Some(Attention::Input),
        RunState::Waiting {
            reason: WaitReason::DeviceUnreachable,
        } => Some(Attention::WaitingDevice),
        // M4: a retry backoff is mid-run work, never a terminal/park — it must
        // stay silent (only the final Errored, once retries exhaust, notifies).
        RunState::Retrying { .. } => None,
        _ => None,
    }
}

/// The bounded, MASKED one-liner for a notification. A `session_title` is
/// masked (emails hidden) and appended; absence yields the bare summary.
#[must_use]
pub fn notification_line(attention: Attention, session_title: Option<&str>) -> String {
    let head = match attention {
        Attention::Done => "haider: turn done",
        Attention::Errored => "haider: turn errored",
        Attention::Permission => "haider: needs your approval",
        Attention::Input => "haider: needs your input",
        Attention::WaitingDevice => "haider: waiting on a device",
    };
    let line = match session_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        Some(title) => format!("{head} — {}", mask_text(title)),
        None => head.to_owned(),
    };
    truncate(&line, MAX_NOTIFICATION_LEN)
}

/// Mask identity-like tokens (emails) inside free text, delegating to the ONE
/// masking authority (`format::mask_identity`) so no second dialect exists.
/// Non-identity words pass through unchanged.
#[must_use]
pub fn mask_text(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            if token.contains('@') {
                mask_identity(token)
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap `text` in an OSC 9 desktop-notification sequence. The text is stripped
/// of control bytes (so it can never terminate the sequence early or inject
/// another) and bounded before wrapping.
#[must_use]
pub fn osc9(text: &str) -> String {
    let sanitized = truncate(&strip_control(text), MAX_NOTIFICATION_LEN);
    format!("\u{1b}]9;{sanitized}\u{7}")
}

/// The bytes to write for a notification given whether the sink is a tty. A
/// NON-tty sink yields nothing — no OSC bytes ever reach a pipe/redirect (the
/// non-tty suppression law). This is the one decision both emitters route
/// through, so the law has a single, testable home.
#[must_use]
pub fn osc9_for_tty(text: &str, is_tty: bool) -> Vec<u8> {
    if is_tty {
        osc9(text).into_bytes()
    } else {
        Vec::new()
    }
}

/// Drop control characters (ESC, BEL, and every other C0/C1 control), keeping
/// ordinary printable text and spaces.
fn strip_control(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Truncate to at most `max` chars on a char boundary (never mid-codepoint).
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars().take(max).collect()
    }
}
