//! `haider update`: discovery, verified staging, pair commit, and restart.

pub(crate) mod check_policy;
pub(crate) mod discovery;
pub(crate) mod restart;
pub(crate) mod staging;
pub(crate) mod transaction;
pub(crate) mod tui;
pub(crate) mod tui_restart;

use discovery::{CurlTransport, DiscoveryOutcome, ReleaseSource, compiled_target, discover};
use restart::{detect_incumbent, restart_committed};
use staging::{StageVerifier, SystemStageVerifier, VerifiedStagedPair, stage_release};
use std::process::ExitCode;
use transaction::{
    InstallLayout, NoFaults, PreparedTransaction, SystemInstalledPairVerifier, commit_pair,
};

pub(crate) const EX_USAGE: u8 = 2;
pub(crate) const EX_UNAVAILABLE: u8 = 69;
pub(crate) const EX_SOFTWARE: u8 = 70;
pub(crate) const EX_IOERR: u8 = 74;
pub(crate) const EX_PROTOCOL: u8 = 76;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpdateOptions {
    pub check: bool,
}

/// Read-only W9 release-discovery result consumed by `haider status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateAvailability {
    Current { version: String },
    Available { current: String, latest: String },
}

/// Structured result shared by the CLI command and the live-TUI host. Only
/// `Updated` means the pair commit and daemon restart completed, and therefore
/// only that variant authorizes replacing the running TUI process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateRunOutcome {
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

pub(crate) fn parse_update_options(rest: &[String]) -> Result<UpdateOptions, UpdateError> {
    match rest {
        [] => Ok(UpdateOptions { check: false }),
        [flag] if flag == "--check" => Ok(UpdateOptions { check: true }),
        _ => Err(UpdateError::Usage("usage: haider update [--check]".into())),
    }
}

#[derive(Debug)]
pub(crate) enum UpdateError {
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

pub(crate) async fn update_command(rest: &[String]) -> ExitCode {
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
pub(crate) async fn run_update(options: UpdateOptions) -> Result<UpdateRunOutcome, UpdateError> {
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

/// Performs only W9's list-and-SemVer gate. This function never calls the
/// download, staging, install-layout, transaction, profile, or restart paths.
pub(crate) fn check_update_availability() -> Result<UpdateAvailability, UpdateError> {
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment();
    check_update_availability_with(&mut transport, &source, super::VERSION, target)
}

pub(crate) fn check_update_availability_cancellable(
    cancellation: discovery::DiscoveryCancellation,
) -> Result<UpdateAvailability, UpdateError> {
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment().with_cancellation(cancellation);
    check_update_availability_with(&mut transport, &source, super::VERSION, target)
}

pub(crate) fn check_update_availability_with<T: discovery::UpdateTransport>(
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
/// create a lock or recover/replace either canonical binary.
pub(crate) fn stage_then_acquire<T: discovery::UpdateTransport, V: StageVerifier>(
    transport: &mut T,
    verifier: &V,
    layout: InstallLayout,
    selection: &discovery::ReleaseSelection,
) -> Result<(PreparedTransaction, VerifiedStagedPair), UpdateError> {
    let pair = stage_release(transport, verifier, &layout.dir, selection)?;
    let prepared = PreparedTransaction::acquire(layout)?;
    Ok((prepared, pair))
}
