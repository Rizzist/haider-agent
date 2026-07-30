//! The runtime's browser hop — the ONE place haider launches an external
//! program on the user's behalf (the OAuth authorize URL).
//!
//! Two laws:
//! - the scheme is allow-listed to `http`/`https` BEFORE anything spawns,
//!   so a hostile "authorize URL" can never become an arbitrary-program
//!   launch (`open` on macOS happily executes `file:` and `ssh:` handlers);
//! - `$BROWSER` wins when set (the POSIX convention), which is also what
//!   makes the hop probeable: a PTY probe points `$BROWSER` at a recorder
//!   script and asserts the REAL authorize URL arrived.

use std::process::{Command, Stdio};

/// Build the platform's opener command for `url` without spawning it.
///
/// Errors on a scheme outside http/https — the caller treats that exactly
/// like a failed spawn (honest fallback, never a launch).
pub fn open_url_command(url: &str) -> std::io::Result<Command> {
    open_url_command_with_env(url, &|name| std::env::var(name).ok())
}

/// The env-injected body of [`open_url_command`] — tests hand in a fake
/// environment instead of mutating the process's.
pub fn open_url_command_with_env(
    url: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::io::Result<Command> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open a non-http(s) URL",
        ));
    }
    let mut command = if let Some(browser) = env("BROWSER").filter(|value| !value.trim().is_empty())
    {
        let mut command = Command::new(browser);
        command.arg(url);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        // `start`'s first quoted argument is the window TITLE — the empty
        // string keeps the URL out of that slot.
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Open `url` in the user's browser, detached — the TUI never waits on it
/// and never inherits its stdio (the terminal is ours).
pub fn open_url(url: &str) -> std::io::Result<()> {
    open_url_command(url)?.spawn().map(drop)
}
