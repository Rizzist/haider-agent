//! Thin `haiderd` entry point; lifecycle ownership lives in `haider-daemon`.
//!
//! Exit codes: 0 graceful drain, 130 forced termination (128 + SIGINT
//! convention), otherwise `DaemonError::exit_code` (sysexits; 75 = a daemon
//! for this profile is already running).

use haider_daemon::{DaemonConfig, ShutdownOutcome, run_with_signals};
use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("haiderd: {message}");
            return ExitCode::from(64);
        }
    };
    match run_with_signals(config).await {
        Ok(ShutdownOutcome::Graceful) => ExitCode::SUCCESS,
        Ok(ShutdownOutcome::Forced) => ExitCode::from(130),
        Err(error) => {
            eprintln!("haiderd: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// `haiderd [--profile <id> --store-dir <path> --runtime-dir <path>]`.
///
/// All three identity flags together, or none: a bare `haiderd` resolves the
/// SAME shared profile `haider` resolves (R8's one-resolver law), while a
/// spawning `haider` passes the exact resolved values explicitly. A partial
/// set is refused — mixing resolved and explicit identity could bind a
/// socket for one profile against another profile's store. Every other knob
/// keeps its `DaemonConfig` default.
fn parse_args(args: impl Iterator<Item = String>) -> Result<DaemonConfig, String> {
    let mut profile = None;
    let mut store_dir = None;
    let mut runtime_dir = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for `{argument}`"))?;
        match argument.as_str() {
            "--profile" => profile = Some(value),
            "--store-dir" => store_dir = Some(PathBuf::from(value)),
            "--runtime-dir" => runtime_dir = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    match (profile, store_dir, runtime_dir) {
        (Some(profile), Some(store_dir), Some(runtime_dir)) => {
            Ok(DaemonConfig::new(profile, store_dir, runtime_dir))
        }
        (None, None, None) => {
            let env = haider_client::ProfileEnv::capture();
            let resolved = haider_client::resolve_profile(&env)
                .map_err(|error| format!("cannot resolve profile: {error}"))?;
            Ok(DaemonConfig::new(
                resolved.profile_id,
                resolved.store_dir,
                resolved.runtime_dir,
            ))
        }
        _ => Err(
            "--profile, --store-dir, and --runtime-dir must be given together (or all omitted \
             to resolve the shared default profile)"
                .into(),
        ),
    }
}
