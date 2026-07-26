//! Thin `haiderd` entry point; lifecycle ownership lives in `haider-daemon`.

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
    Ok(DaemonConfig::new(
        profile.ok_or_else(|| "--profile is required".to_owned())?,
        store_dir.ok_or_else(|| "--store-dir is required".to_owned())?,
        runtime_dir.ok_or_else(|| "--runtime-dir is required".to_owned())?,
    ))
}
