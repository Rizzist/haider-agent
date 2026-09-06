//! `haider update`: discovery, verified staging, bundle commit, and restart.

pub mod check_policy;
pub mod discovery;
pub mod members;
pub mod restart;
pub mod staging;
pub mod transaction;
pub mod tui_restart;

use discovery::{CurlTransport, DiscoveryOutcome, ReleaseSource, compiled_target, discover};
use restart::{detect_incumbent, restart_committed};
use staging::{StageVerifier, SystemStageVerifier, VerifiedStagedPair, stage_release};
use std::process::ExitCode;
use transaction::{
    InstallLayout, NoFaults, PreparedTransaction, SystemInstalledPairVerifier, commit_pair,
};

pub const EX_USAGE: u8 = 2;
pub const EX_UNAVAILABLE: u8 = 69;
pub const EX_SOFTWARE: u8 = 70;
pub const EX_IOERR: u8 = 74;
pub const EX_PROTOCOL: u8 = 76;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOptions {
    pub check: bool,
}

/// Read-only W9 release-discovery result consumed by `haider status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAvailability {
    Current { version: String },
    Available { current: String, latest: String },
}

/// Structured result shared by the CLI command and the live-TUI host. Only
/// `Updated` means the bundle commit and daemon restart completed, and therefore
/// only that variant authorizes replacing the running TUI process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRunOutcome {
    Current { version: String },
    Available { current: String, latest: String },
    Updated { version: String },
}

impl UpdateRunOutcome {
    fn cli_message(&self) -> String {
        match self {
            Self::Current { version } => format!("haider {version} is current"),
            Self::Available { current, latest } => {
                format!("haider {current}; update available: {latest}")
            }
            Self::Updated { version } => format!("updated haider to {version}"),
        }
    }
}

pub fn parse_update_options(rest: &[String]) -> Result<UpdateOptions, UpdateError> {
    match rest {
        [] => Ok(UpdateOptions { check: false }),
        [flag] if flag == "--check" => Ok(UpdateOptions { check: true }),
        _ => Err(UpdateError::Usage("usage: haider update [--check]".into())),
    }
}

#[derive(Debug)]
pub enum UpdateError {
    Usage(String),
    Network(String),
    Io(String),
    Refused(String),
    Health(String),
    RestartTimeout(String),
    Internal(String),
}

impl UpdateError {
    pub fn io(operation: &'static str, error: std::io::Error) -> Self {
        Self::Io(format!("{operation}: {error}"))
    }

    pub fn network(message: String) -> Self {
        Self::Network(message)
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => EX_USAGE,
            Self::Network(_) => EX_UNAVAILABLE,
            Self::Io(_) => EX_IOERR,
            Self::Health(_) => EX_PROTOCOL,
            Self::Refused(_) | Self::RestartTimeout(_) | Self::Internal(_) => EX_SOFTWARE,
        }
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message)
            | Self::Network(message)
            | Self::Io(message)
            | Self::Refused(message)
            | Self::Health(message)
            | Self::RestartTimeout(message)
            | Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for UpdateError {}

pub async fn update_command(rest: &[String]) -> ExitCode {
    let options = match parse_update_options(rest) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("haider update: {error}");
            return ExitCode::from(error.exit_code());
        }
    };
    match run_update_with_reporter(options, |message| eprintln!("haider update: {message}")).await {
        Ok(outcome) => {
            println!("{}", outcome.cli_message());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("haider update: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// Runs the existing verified update transaction without writing to stdout or
/// stderr. Embedded callers surface progress and errors through their own UI.
pub async fn run_update(options: UpdateOptions) -> Result<UpdateRunOutcome, UpdateError> {
    run_update_with_reporter(options, |_| {}).await
}

async fn run_update_with_reporter(
    options: UpdateOptions,
    mut report: impl FnMut(&str),
) -> Result<UpdateRunOutcome, UpdateError> {
    // Discovery and the gate precede profile resolution and every local
    // mutation. `resolve_profile` creates directories, so moving it above
    // this point would violate both --check and equal-version no-op laws.
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment();
    let outcome = discover_update_with(&mut transport, &source, super::VERSION, target)?;
    let selection = match outcome {
        DiscoveryOutcome::Current(version) => {
            return Ok(UpdateRunOutcome::Current {
                version: version.to_string(),
            });
        }
        DiscoveryOutcome::Update(selection) if options.check => {
            return Ok(UpdateRunOutcome::Available {
                current: super::VERSION.to_owned(),
                latest: selection.version.to_string(),
            });
        }
        DiscoveryOutcome::Update(selection) => selection,
    };

    let layout = InstallLayout::running()?;
    let (prepared, pair) =
        stage_then_acquire(&mut transport, &SystemStageVerifier, layout, &selection)?;

    // Only an admitted, completely verified stage may resolve a profile or
    // interact with a daemon. Missing/refused means "leave it stopped";
    // every other connection failure is a refusal, never an auto-spawn cue.
    let profile = haider_client::resolve_profile(&haider_client::ProfileEnv::capture())
        .map_err(|error| UpdateError::Io(format!("cannot resolve current profile: {error}")))?;
    let incumbent = detect_incumbent(&profile).await?;
    report(
        "active turns on this profile may be cancelled by drain; daemons for other profiles are \
         outside this update and are not restarted",
    );

    let mut committed = commit_pair(
        prepared,
        pair,
        &NoFaults,
        &SystemInstalledPairVerifier,
        super::VERSION,
    )?;
    restart_committed(&mut committed, incumbent, &profile).await?;
    Ok(UpdateRunOutcome::Updated {
        version: selection.version.to_string(),
    })
}

/// Completes a legacy updater's compatibility entrypoint migration using the
/// exact embedded thin/payload build and the canonical sibling daemon. Staging
/// and all smoke checks complete before transaction acquisition, profile
/// creation or incumbent-daemon interaction.
pub async fn install_embedded_bundle(
    thin: &[u8],
    payload: &[u8],
) -> Result<UpdateRunOutcome, UpdateError> {
    let layout = InstallLayout::running()?;
    let bundle = staging::stage_embedded_bundle(
        thin,
        payload,
        &layout.haiderd,
        &layout.dir,
        super::VERSION,
    )?;
    let prepared = PreparedTransaction::acquire(layout)?;
    let profile = haider_client::resolve_profile(&haider_client::ProfileEnv::capture())
        .map_err(|error| UpdateError::Io(format!("cannot resolve current profile: {error}")))?;
    let incumbent = detect_incumbent(&profile).await?;
    let mut committed = commit_pair(
        prepared,
        bundle,
        &NoFaults,
        &SystemInstalledPairVerifier,
        super::VERSION,
    )?;
    restart_committed(&mut committed, incumbent, &profile).await?;
    Ok(UpdateRunOutcome::Updated {
        version: super::VERSION.to_owned(),
    })
}

/// Installs a staged package without creating a profile or interacting with a
/// running profile daemon. The unpacker verifies the archive checksum first;
/// this boundary verifies every copied executable and uses the same durable
/// member transaction as self-update. Native Windows crash durability remains
/// limited by the platform directory-sync seam, which is currently a no-op.
pub fn install_bundle_from_directory(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<(), UpdateError> {
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    // Match the direct installer's public prefix; private transaction assets
    // below it still set their own owner-only modes. Existing dirs are intact.
    haider_platform::configure_directory_mode(&mut directory, 0o755);
    directory
        .create(destination)
        .map_err(|error| UpdateError::io("create installation directory", error))?;
    let layout = InstallLayout::for_install_directory(destination.to_path_buf())?;
    let bundle = staging::stage_directory_bundle(source, &layout.dir, super::VERSION)?;
    let prepared = PreparedTransaction::acquire(layout)?;
    let mut committed = commit_pair(
        prepared,
        bundle,
        &NoFaults,
        &SystemInstalledPairVerifier,
        super::VERSION,
    )?;
    committed.finalize()
}

/// Performs only W9's list-and-SemVer gate. This function never calls the
/// download, staging, install-layout, transaction, profile, or restart paths.
pub fn check_update_availability() -> Result<UpdateAvailability, UpdateError> {
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment();
    check_update_availability_with(&mut transport, &source, super::VERSION, target)
}

pub fn check_update_availability_cancellable(
    cancellation: discovery::DiscoveryCancellation,
) -> Result<UpdateAvailability, UpdateError> {
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment().with_cancellation(cancellation);
    check_update_availability_with(&mut transport, &source, super::VERSION, target)
}

pub fn check_update_availability_with<T: discovery::UpdateTransport>(
    transport: &mut T,
    source: &ReleaseSource,
    current: &str,
    target: &str,
) -> Result<UpdateAvailability, UpdateError> {
    match discover_update_with(transport, source, current, target)? {
        DiscoveryOutcome::Current(version) => Ok(UpdateAvailability::Current {
            version: version.to_string(),
        }),
        DiscoveryOutcome::Update(selection) => Ok(UpdateAvailability::Available {
            current: current.to_owned(),
            latest: selection.version.to_string(),
        }),
    }
}

fn discover_update_with<T: discovery::UpdateTransport>(
    transport: &mut T,
    source: &ReleaseSource,
    current: &str,
    target: &str,
) -> Result<DiscoveryOutcome, UpdateError> {
    discover(transport, source, current, target)
}

/// Builds the immutable, fully verified staging capability before entering
/// the update lock/recovery/commit slice.
///
/// MUTATION SAFETY: a partial transfer, checksum mismatch, archive refusal,
/// or staged verification failure returns before `PreparedTransaction` can
/// create a lock or recover/replace any canonical binary.
pub fn stage_then_acquire<T: discovery::UpdateTransport, V: StageVerifier>(
    transport: &mut T,
    verifier: &V,
    layout: InstallLayout,
    selection: &discovery::ReleaseSelection,
) -> Result<(PreparedTransaction, VerifiedStagedPair), UpdateError> {
    let pair = stage_release(transport, verifier, &layout.dir, selection)?;
    let prepared = PreparedTransaction::acquire(layout)?;
    Ok((prepared, pair))
}
