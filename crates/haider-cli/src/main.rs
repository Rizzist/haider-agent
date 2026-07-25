//! `haider` — the Haider Code harness binary.
//!
//! v0.0.1: role dispatch skeleton. Real roles land per the build guide
//! (B4 thin headless run, then daemon/tui/attach/rpc). Argument parsing is
//! deliberately dependency-free at this stage.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every workspace crate, asserted linkable by the self-test.
const CRATES: [&str; 9] = [
    haider_protocol::CRATE_NAME,
    haider_store::CRATE_NAME,
    haider_core::CRATE_NAME,
    haider_provider::CRATE_NAME,
    haider_tools::CRATE_NAME,
    haider_verify::CRATE_NAME,
    haider_accounts::CRATE_NAME,
    haider_rpc::CRATE_NAME,
    haider_tui::CRATE_NAME,
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("haider {VERSION}");
            ExitCode::SUCCESS
        }
        Some("self-test") => self_test(),
        Some(other) => {
            eprintln!("haider: unknown command `{other}` (v0.0.1 supports: --version, self-test)");
            ExitCode::from(2)
        }
        None => {
            println!("haider {VERSION} — scaffold build; run `haider self-test`");
            ExitCode::SUCCESS
        }
    }
}

/// Offline, ephemeral, deterministic. Structured JSON on stdout.
/// Grows with the system per the build guide; never touches a real profile.
fn self_test() -> ExitCode {
    let checks: Vec<String> = CRATES
        .iter()
        .map(|name| format!(r#"{{"name":"link:{name}","ok":true}}"#))
        .collect();
    println!(
        r#"{{"schema":"haider.selftest.v0","version":"{VERSION}","ok":true,"checks":[{}]}}"#,
        checks.join(",")
    );
    ExitCode::SUCCESS
}
