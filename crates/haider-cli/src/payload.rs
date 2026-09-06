//! Sibling process handoff. Headless dispatch never enters the TUI path.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn sibling(name: &str) -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

/// Serialize interactive lookup/exec with the updater's exclusive OS lock.
/// There is no negotiated RPC connection at this point. On Unix exec opens
/// the chosen image before closing this CLOEXEC descriptor, so replacement
/// cannot race version validation. Read-only managed installs need no new lock.
fn install_read_lock(executable: &Path) -> io::Result<Option<File>> {
    let path = executable.with_file_name(".haider-update.lock");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("update lock is a symlink"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    haider_platform::configure_file_mode(&mut options, 0o600);
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || !haider_platform::metadata_is_current_user(&metadata)
        || haider_platform::metadata_mode(&metadata) & 0o077 != 0
    {
        return Err(io::Error::other(
            "update lock is not an owner-only regular file",
        ));
    }
    file.lock_shared()?;
    Ok(Some(file))
}

pub(super) fn launch() -> ExitCode {
    let result = (|| {
        let executable = sibling("haider-tui")?;
        let _lock = install_read_lock(&executable)?;
        haider_client::payload_identity::verify_payload(&executable)?;
        let mut command = Command::new(&executable);
        command
            .args(std::env::args_os().skip(1))
            .env("HAIDER_TUI_LAUNCH_VERSION", super::VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0(
                std::env::args_os()
                    .next()
                    .unwrap_or_else(|| "haider".into()),
            );
            Err(command.exec())
        }
        #[cfg(not(unix))]
        {
            // Windows has no exec. Keep inherited console/std handles and
            // return the child's full exit code; release the lookup lock once
            // CreateProcess has opened the payload so in-TUI update cannot
            // deadlock on the waiting launcher's shared lock.
            let mut child = command.spawn()?;
            drop(_lock);
            let status = child.wait()?;
            #[cfg(windows)]
            super::hold_explorer_console_on_failure(if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
            std::process::exit(status.code().unwrap_or(70));
        }
    })();
    let Err(error): io::Result<()> = result else {
        return ExitCode::SUCCESS;
    };
    eprintln!(
        "haider: cannot start matching interactive payload: {error}; reinstall the complete haider bundle"
    );
    ExitCode::from(69)
}

pub(super) fn self_test() -> ExitCode {
    let result = (|| {
        let daemon = sibling("haiderd")?;
        let _lock = install_read_lock(&daemon)?;
        haider_client::payload_identity::verify_payload(&sibling("haider-tui")?)?;
        // The actual fake-provider exercise is daemon-owned. This path never
        // executes the TUI; daemon self-test checks its build marker as data.
        let mut command = Command::new(daemon);
        command.arg("--client-self-test");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(command.exec())
        }
        #[cfg(not(unix))]
        {
            let status = command.status()?;
            #[cfg(windows)]
            super::hold_explorer_console_on_failure(if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
            std::process::exit(status.code().unwrap_or(70));
        }
    })();
    let Err(error): io::Result<()> = result else {
        return ExitCode::SUCCESS;
    };
    eprintln!("haider self-test: cannot run daemon diagnostic: {error}");
    ExitCode::from(69)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn launch_lock_is_compatible_with_the_updater_owner_only_contract() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lock = install_read_lock(&dir.path().join("haider-tui"))
            .expect("launch lock")
            .expect("writable install");
        let metadata = lock.metadata().expect("lock metadata");
        assert!(metadata.is_file());
        assert_eq!(haider_platform::metadata_mode(&metadata) & 0o077, 0);
        drop(lock);
        let exclusive = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join(".haider-update.lock"))
            .expect("updater open");
        exclusive.try_lock().expect("launch lock released");
    }
}
