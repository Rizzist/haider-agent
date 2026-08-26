//! Daemon-owned installation engine for one typed-agent CLI requirement.
//!
//! The authored contract supplies only the exact required program token. If
//! it is absent, this module selects an executable plus argv from a closed,
//! platform-specific catalog. No authored value can become shell source or
//! alter the installer arguments.

use async_trait::async_trait;
use haider_protocol::typed_agent::{TYPED_AGENT_INSTALL_ERROR_MAX_BYTES, TypedAgentRequiredCli};
use std::fmt::{Display, Formatter};
use std::io;
use std::process::Stdio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedAgentInstallDisposition {
    AlreadyPresent,
    Installed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedAgentInstallerErrorCode {
    InvalidRequiredCli,
    UnsupportedRecipe,
    InstallerLaunchFailed,
    InstallerTimedOut,
    InstallerExitedNonZero,
    MissingAfterInstall,
}

/// Bounded failure safe to copy into the durable install-job error field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedAgentInstallerError {
    pub(crate) code: TypedAgentInstallerErrorCode,
    pub(crate) message: String,
    pub(crate) exit_code: Option<i32>,
}

impl TypedAgentInstallerError {
    fn new(
        code: TypedAgentInstallerErrorCode,
        message: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        let mut message = message.into();
        while message.len() > TYPED_AGENT_INSTALL_ERROR_MAX_BYTES {
            message.pop();
        }
        Self {
            code,
            message,
            exit_code,
        }
    }
}

impl Display for TypedAgentInstallerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TypedAgentInstallerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedInstallCommand {
    executable: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstallCommandOutcome {
    success: bool,
    exit_code: Option<i32>,
}

pub(crate) trait CliPresenceProbe: Send + Sync {
    fn is_present(&self, program: &str) -> bool;
}

#[async_trait]
pub(crate) trait InstallCommandRunner: Send + Sync {
    async fn run(&self, command: &TrustedInstallCommand) -> io::Result<InstallCommandOutcome>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PathCliPresenceProbe;

impl CliPresenceProbe for PathCliPresenceProbe {
    fn is_present(&self, program: &str) -> bool {
        haider_platform::program_on_path(program)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TokioInstallCommandRunner;

#[async_trait]
impl InstallCommandRunner for TokioInstallCommandRunner {
    async fn run(&self, recipe: &TrustedInstallCommand) -> io::Result<InstallCommandOutcome> {
        const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
        let mut command = tokio::process::Command::new(&recipe.executable);
        command
            .args(&recipe.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(INSTALL_TIMEOUT, command.status())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "trusted typed-agent installer timed out",
                )
            })??;
        Ok(InstallCommandOutcome {
            success: status.success(),
            exit_code: status.code(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallerPlatform {
    MacOs,
    Linux,
    Windows,
    Unsupported,
}

impl InstallerPlatform {
    const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unsupported
        }
    }
}

/// Isolated installer; persistence and scheduling remain the caller's job.
pub(crate) struct TypedAgentCliInstaller<P = PathCliPresenceProbe, R = TokioInstallCommandRunner> {
    platform: InstallerPlatform,
    probe: P,
    runner: R,
}

impl TypedAgentCliInstaller<PathCliPresenceProbe, TokioInstallCommandRunner> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            platform: InstallerPlatform::current(),
            probe: PathCliPresenceProbe,
            runner: TokioInstallCommandRunner,
        }
    }
}

impl Default for TypedAgentCliInstaller<PathCliPresenceProbe, TokioInstallCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P, R> TypedAgentCliInstaller<P, R>
where
    P: CliPresenceProbe,
    R: InstallCommandRunner,
{
    #[cfg(test)]
    fn with_dependencies(platform: InstallerPlatform, probe: P, runner: R) -> Self {
        Self {
            platform,
            probe,
            runner,
        }
    }

    /// Ensure one validated CLI requirement is present. The second probe is
    /// authoritative: a successful package-manager exit without the declared
    /// executable on PATH is still a failed install.
    pub(crate) async fn install(
        &self,
        required: &TypedAgentRequiredCli,
    ) -> Result<TypedAgentInstallDisposition, TypedAgentInstallerError> {
        required.validate().map_err(|_| {
            TypedAgentInstallerError::new(
                TypedAgentInstallerErrorCode::InvalidRequiredCli,
                "typed-agent required CLI contract is invalid",
                None,
            )
        })?;
        if self.probe.is_present(&required.program) {
            return Ok(TypedAgentInstallDisposition::AlreadyPresent);
        }

        let recipe = trusted_recipe(self.platform, &required.program).ok_or_else(|| {
            TypedAgentInstallerError::new(
                TypedAgentInstallerErrorCode::UnsupportedRecipe,
                format!(
                    "no trusted installer recipe is available for required CLI `{}` on this platform",
                    required.program
                ),
                None,
            )
        })?;
        let outcome = self.runner.run(&recipe).await.map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                return TypedAgentInstallerError::new(
                    TypedAgentInstallerErrorCode::InstallerTimedOut,
                    format!(
                        "trusted installer `{}` timed out while installing `{}`",
                        recipe.executable, required.program
                    ),
                    None,
                );
            }
            let reason = match error.kind() {
                io::ErrorKind::NotFound => "trusted installer executable is not available",
                io::ErrorKind::PermissionDenied => {
                    "trusted installer executable is not permitted on this device"
                }
                _ => "trusted installer executable could not be launched",
            };
            TypedAgentInstallerError::new(
                TypedAgentInstallerErrorCode::InstallerLaunchFailed,
                format!("{reason}: {}", recipe.executable),
                None,
            )
        })?;
        if !outcome.success {
            return Err(TypedAgentInstallerError::new(
                TypedAgentInstallerErrorCode::InstallerExitedNonZero,
                match outcome.exit_code {
                    Some(code) => format!(
                        "trusted installer `{}` exited with status {code} while installing `{}`",
                        recipe.executable, required.program
                    ),
                    None => format!(
                        "trusted installer `{}` ended without an exit status while installing `{}`",
                        recipe.executable, required.program
                    ),
                },
                outcome.exit_code,
            ));
        }
        if !self.probe.is_present(&required.program) {
            return Err(TypedAgentInstallerError::new(
                TypedAgentInstallerErrorCode::MissingAfterInstall,
                format!(
                    "trusted installer completed but required CLI `{}` is still absent from PATH",
                    required.program
                ),
                None,
            ));
        }
        Ok(TypedAgentInstallDisposition::Installed)
    }
}

fn trusted_recipe(platform: InstallerPlatform, program: &str) -> Option<TrustedInstallCommand> {
    let package = match program {
        "rg" => ("ripgrep", "BurntSushi.ripgrep.MSVC"),
        "jq" => ("jq", "jqlang.jq"),
        "yt-dlp" => ("yt-dlp", "yt-dlp.yt-dlp"),
        "ffmpeg" => ("ffmpeg", "Gyan.FFmpeg"),
        _ => return None,
    };
    let (executable, args): (&str, Vec<&str>) = match platform {
        InstallerPlatform::MacOs if cfg!(target_arch = "aarch64") => {
            ("/opt/homebrew/bin/brew", vec!["install", package.0])
        }
        InstallerPlatform::MacOs => ("/usr/local/bin/brew", vec!["install", package.0]),
        InstallerPlatform::Linux => ("/usr/bin/apt-get", vec!["install", "--yes", package.0]),
        InstallerPlatform::Windows => (
            "winget",
            vec![
                "install",
                "--id",
                package.1,
                "--exact",
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
            ],
        ),
        InstallerPlatform::Unsupported => return None,
    };
    Some(TrustedInstallCommand {
        executable: executable.into(),
        args: args.into_iter().map(str::to_owned).collect(),
    })
}

#[cfg(test)]
#[path = "typed_agent_installer_tests.rs"]
mod typed_agent_installer_tests;
