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
//! Failure is a FLASH, never a crash: a spawn or write error reports
//! `false` and the caller shows `· copy failed`. A missing `pbcopy`
//! (non-macOS host) degrades the same way — OSC 52 still goes out, so the
//! copy can still land via the terminal.

use std::io::Write as _;
use std::process::{Command, Stdio};

use base64::Engine as _;

/// Hand `text` to the local clipboard via `pbcopy`. Returns `true` when the
/// handoff succeeded (spawn + stdin write); the child is reaped on a
/// detached thread so the event loop never waits on it.
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
    let ok = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    // Reap off-loop: pbcopy exits as soon as stdin closes, but wait()ing
    // here would still block the event loop on process teardown.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    ok
}

/// The OSC 52 clipboard-set sequence for `text` (`c` = the system
/// clipboard selection). Terminals that support it (iTerm2, kitty, wezterm,
/// tmux with `set-clipboard`) copy on sight; the rest ignore it silently.
#[must_use]
pub fn osc52(text: &str) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{payload}\x07")
}
