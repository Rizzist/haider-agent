//! Process hand-off after an in-TUI update has committed successfully.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// The platform-specific process hand-off selected for a restarted TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartMode {
    /// Replace this process after the terminal guard has been dropped.
    Exec,
    /// Start a detached successor, then let this process exit cleanly.
    DetachedSpawn,
}

/// A pure description of the successor process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiRestartPlan {
    pub executable: PathBuf,
    /// Arguments after argv[0], preserved byte-for-byte from this process.
    pub args: Vec<OsString>,
    pub mode: RestartMode,
}

/// Construct a plan for the current target without starting a process.
#[must_use]
pub(crate) fn restart_plan(executable: PathBuf, original_argv: &[OsString]) -> TuiRestartPlan {
    restart_plan_for(executable, original_argv, cfg!(windows))
}

/// Platform-parameterized seam used to pin both process strategies on every
/// test host. `original_argv` includes argv[0]; the successor program is the
/// newly committed canonical binary, so only the remaining arguments are
/// forwarded.
#[must_use]
pub(crate) fn restart_plan_for(
    executable: PathBuf,
    original_argv: &[OsString],
    windows: bool,
) -> TuiRestartPlan {
    TuiRestartPlan {
        executable,
        args: original_argv.iter().skip(1).cloned().collect(),
        mode: if windows {
            RestartMode::DetachedSpawn
        } else {
            RestartMode::Exec
        },
    }
}

/// Execute a previously constructed restart plan.
///
/// On Unix success never returns. On Windows the successor is detached and
/// success returns so the caller can exit normally.
pub(crate) fn execute_restart(plan: TuiRestartPlan) -> std::io::Result<()> {
    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    match plan.mode {
        RestartMode::Exec => exec(command),
        RestartMode::DetachedSpawn => spawn_detached(command),
    }
}

#[cfg(unix)]
fn exec(mut command: Command) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;

    Err(command.exec())
}

#[cfg(not(unix))]
fn exec(_command: Command) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exec restart is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn spawn_detached(mut command: Command) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    // A new process group lets the old process exit without coupling control
    // signals, while inherited console/std handles keep the restarted TUI
    // attached to the terminal the guard just restored. DETACHED_PROCESS
    // would sever that console and make an interactive restart unusable.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP).spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(_command: Command) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "detached restart is unavailable on this platform",
    ))
}
