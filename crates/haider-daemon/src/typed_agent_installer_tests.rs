#![allow(clippy::expect_used, clippy::unwrap_used)]

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
    fn returning(outcomes: impl IntoIterator<Item = io::Result<InstallCommandOutcome>>) -> Self {
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
