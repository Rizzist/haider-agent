//! `haider update`: discovery, verified staging, pair commit, and restart.

pub(crate) mod discovery;
pub(crate) mod restart;
pub(crate) mod staging;
pub(crate) mod transaction;

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
    match run_update(options).await {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("haider update: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

async fn run_update(options: UpdateOptions) -> Result<String, UpdateError> {
    // Discovery and the gate precede profile resolution and every local
    // mutation. `resolve_profile` creates directories, so moving it above
    // this point would violate both --check and equal-version no-op laws.
    let source = ReleaseSource::production()?;
    let target = compiled_target()?;
    let mut transport = CurlTransport::from_environment();
    let outcome = discover(&mut transport, &source, super::VERSION, target)?;
    let selection = match outcome {
        DiscoveryOutcome::Current(version) => {
            return Ok(format!("haider {version} is current"));
        }
        DiscoveryOutcome::Update(selection) if options.check => {
            return Ok(format!(
                "haider {}; update available: {}",
                super::VERSION,
                selection.version
            ));
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
    eprintln!(
        "haider update: active turns on this profile may be cancelled by drain; \
         daemons for other profiles are outside this update and are not restarted"
    );

    let mut committed = commit_pair(
        prepared,
        pair,
        &NoFaults,
        &SystemInstalledPairVerifier,
        super::VERSION,
    )?;
    restart_committed(&mut committed, incumbent, &profile).await?;
    Ok(format!("updated haider to {}", selection.version))
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
