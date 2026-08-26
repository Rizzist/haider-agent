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
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeProbe {
        results: Mutex<VecDeque<bool>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProbe {
        fn returning(results: impl IntoIterator<Item = bool>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CliPresenceProbe for FakeProbe {
        fn is_present(&self, program: &str) -> bool {
            self.calls.lock().expect("probe calls").push(program.into());
            self.results
                .lock()
                .expect("probe results")
                .pop_front()
                .unwrap_or(false)
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        outcomes: Mutex<VecDeque<io::Result<InstallCommandOutcome>>>,
        calls: Mutex<Vec<TrustedInstallCommand>>,
    }

    impl FakeRunner {
        fn returning(
            outcomes: impl IntoIterator<Item = io::Result<InstallCommandOutcome>>,
        ) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl InstallCommandRunner for FakeRunner {
        async fn run(&self, command: &TrustedInstallCommand) -> io::Result<InstallCommandOutcome> {
            self.calls
                .lock()
                .expect("runner calls")
                .push(command.clone());
            self.outcomes
                .lock()
                .expect("runner outcomes")
                .pop_front()
                .unwrap_or_else(|| Err(io::Error::other("missing fake outcome")))
        }
    }

    fn required(program: &str) -> TypedAgentRequiredCli {
        TypedAgentRequiredCli {
            program: program.into(),
        }
    }

    fn success() -> io::Result<InstallCommandOutcome> {
        Ok(InstallCommandOutcome {
            success: true,
            exit_code: Some(0),
        })
    }

    #[test]
    fn recipe_catalog_is_closed_and_platform_specific() {
        assert_eq!(
            trusted_recipe(InstallerPlatform::Linux, "rg"),
            Some(TrustedInstallCommand {
                executable: "/usr/bin/apt-get".into(),
                args: vec!["install".into(), "--yes".into(), "ripgrep".into()],
            })
        );
        assert_eq!(
            trusted_recipe(InstallerPlatform::Windows, "ffmpeg"),
            Some(TrustedInstallCommand {
                executable: "winget".into(),
                args: [
                    "install",
                    "--id",
                    "Gyan.FFmpeg",
                    "--exact",
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            })
        );
        assert_eq!(trusted_recipe(InstallerPlatform::MacOs, "cargo"), None);
    }

    #[tokio::test]
    async fn already_present_program_skips_the_installer() {
        let installer = TypedAgentCliInstaller::with_dependencies(
            InstallerPlatform::MacOs,
            FakeProbe::returning([true]),
            FakeRunner::default(),
        );

        assert_eq!(
            installer.install(&required("yt-dlp")).await,
            Ok(TypedAgentInstallDisposition::AlreadyPresent)
        );
        assert_eq!(
            *installer.probe.calls.lock().expect("probe calls"),
            ["yt-dlp"]
        );
        assert!(
            installer
                .runner
                .calls
                .lock()
                .expect("runner calls")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn catalog_runs_exact_argv_and_verifies_path_after_success() {
        let installer = TypedAgentCliInstaller::with_dependencies(
            InstallerPlatform::MacOs,
            FakeProbe::returning([false, true]),
            FakeRunner::returning([success()]),
        );

        assert_eq!(
            installer.install(&required("yt-dlp")).await,
            Ok(TypedAgentInstallDisposition::Installed)
        );
        assert_eq!(
            *installer.runner.calls.lock().expect("runner calls"),
            [TrustedInstallCommand {
                executable: if cfg!(target_arch = "aarch64") {
                    "/opt/homebrew/bin/brew".into()
                } else {
                    "/usr/local/bin/brew".into()
                },
                args: vec!["install".into(), "yt-dlp".into()],
            }]
        );
        assert_eq!(
            *installer.probe.calls.lock().expect("probe calls"),
            ["yt-dlp", "yt-dlp"]
        );
    }

    #[tokio::test]
    async fn unsupported_program_is_rejected_without_process_execution() {
        let installer = TypedAgentCliInstaller::with_dependencies(
            InstallerPlatform::Linux,
            FakeProbe::returning([false]),
            FakeRunner::default(),
        );

        let error = installer
            .install(&required("unknown-tool"))
            .await
            .expect_err("unknown program has no trusted recipe");
        assert_eq!(error.code, TypedAgentInstallerErrorCode::UnsupportedRecipe);
        assert!(
            installer
                .runner
                .calls
                .lock()
                .expect("runner calls")
                .is_empty()
        );
        assert!(error.message.len() <= TYPED_AGENT_INSTALL_ERROR_MAX_BYTES);
    }

    #[tokio::test]
    async fn successful_process_without_verified_program_is_a_typed_failure() {
        let installer = TypedAgentCliInstaller::with_dependencies(
            InstallerPlatform::Windows,
            FakeProbe::returning([false, false]),
            FakeRunner::returning([success()]),
        );

        let error = installer
            .install(&required("jq"))
            .await
            .expect_err("PATH verification must remain authoritative");
        assert_eq!(
            error.code,
            TypedAgentInstallerErrorCode::MissingAfterInstall
        );
        assert_eq!(
            *installer.runner.calls.lock().expect("runner calls"),
            [TrustedInstallCommand {
                executable: "winget".into(),
                args: [
                    "install",
                    "--id",
                    "jqlang.jq",
                    "--exact",
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            }]
        );
    }

    #[tokio::test]
    async fn nonzero_installer_exit_is_typed_and_does_not_claim_verification() {
        let installer = TypedAgentCliInstaller::with_dependencies(
            InstallerPlatform::Linux,
            FakeProbe::returning([false]),
            FakeRunner::returning([Ok(InstallCommandOutcome {
                success: false,
                exit_code: Some(100),
            })]),
        );

        let error = installer
            .install(&required("ffmpeg"))
            .await
            .expect_err("nonzero package-manager exit must fail");
        assert_eq!(
            error.code,
            TypedAgentInstallerErrorCode::InstallerExitedNonZero
        );
        assert_eq!(error.exit_code, Some(100));
        assert_eq!(
            *installer.probe.calls.lock().expect("probe calls"),
            ["ffmpeg"]
        );
    }
}
