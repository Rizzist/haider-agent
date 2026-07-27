//! Auto-copy for the in-app selection (owner item 9).
//!
//! ORDER, documented: (1) `pbcopy` — the authoritative LOCAL macOS
//! clipboard; (2) OSC 52 — best-effort, ALWAYS emitted after, so a remote
//! or embedded terminal viewing this TUI (ssh, a web terminal) can mirror
//! the copy into its own host clipboard. Neither step may stall the event
//! loop: `pbcopy` is spawned with a piped stdin (a screen's worth of text
//! is far below the pipe buffer, so the write cannot block) and reaped on a
//! detached thread; OSC 52 is a single buffered write the caller flushes
//! with the frame.
//!
//! Failure is a FLASH, never a crash: an unconfirmed local copy reports
//! `false` and the caller words the flash honestly (OSC 52 already went
//! out, so the copy may still land via the terminal). A missing `pbcopy`
//! (non-macOS host) degrades the same way.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// How long [`copy_local`] will poll for `pbcopy`'s exit before declaring
/// the local copy unconfirmed. pbcopy exits within a few ms of stdin
/// closing; the bound only exists so a wedged child cannot stall the
/// event loop.
const CONFIRM_BOUND: Duration = Duration::from_millis(300);

/// Hand `text` to the local clipboard via `pbcopy`. Returns `true` ONLY
/// once the child's EXIT STATUS confirms success (review TUI4.1 P3-5 —
/// success used to be claimed after spawn + stdin write, so a failing
/// `pbcopy` still flashed `· copied`). The wait is bounded: a child that
/// has not exited within [`CONFIRM_BOUND`] is reaped on a detached thread
/// and the copy reports UNCONFIRMED (`false`) — a bounded process-exit
/// poll, not a synchronization sleep.
#[must_use]
pub fn copy_local(text: &str) -> bool {
    let mut child = match Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    // stdin is closed (dropped) either way — pbcopy sees EOF and exits.
    let deadline = Instant::now() + CONFIRM_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return wrote && status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Timed out or errored: reap off-loop, report unconfirmed.
            _ => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return false;
            }
        }
    }
}

/// The OSC 52 clipboard-set sequence for `text` (`c` = the system
/// clipboard selection). Terminals that support it (iTerm2, kitty, wezterm,
/// tmux with `set-clipboard`) copy on sight; the rest ignore it silently.
#[must_use]
pub fn osc52(text: &str) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{payload}\x07")
}
