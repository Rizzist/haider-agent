//! `haider` — the Haider Code harness binary.

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_tui::app::AppModel;
use haider_tui::demo_store::DemoStore;
use haider_tui::runtime::{LiveExit, detect_system_theme, run_demo, run_demo_plain, run_live};
use haider_tui::sanctum::SanctumTier;
use haider_tui::settings::SettingsStore;
use haider_tui::theme::ThemeChoice;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

pub(crate) mod account;
pub(crate) mod automation;
pub(crate) mod daemon;
pub(crate) mod export;
pub(crate) mod graph;
pub(crate) mod hooks;
pub(crate) mod lockdown;
pub(crate) mod models;
pub(crate) mod observe;
pub(crate) mod peer;
pub(crate) mod provider;
pub(crate) mod run;
pub(crate) mod session_config;
pub(crate) mod session_item;
pub(crate) mod session_recover;
pub(crate) mod session_seen;
pub(crate) mod session_workspace;
pub(crate) mod shell_registry;
pub(crate) mod ssh;
pub(crate) mod update;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const EX_UNAVAILABLE: u8 = 69;
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;
const FOOTPRINT_HOLD_ENV: &str = "HAIDER_CLIENT_FOOTPRINT_HOLD_MS";
const MAX_FOOTPRINT_HOLD_MS: u64 = 5 * 60 * 1_000;

/// Every workspace crate, asserted linkable by the self-test.
const CRATES: [&str; 10] = [
    haider_protocol::CRATE_NAME,
    haider_store::CRATE_NAME,
    haider_core::CRATE_NAME,
    haider_provider::CRATE_NAME,
    haider_tools::CRATE_NAME,
    haider_verify::CRATE_NAME,
    haider_accounts::CRATE_NAME,
    haider_rpc::CRATE_NAME,
    haider_client::CRATE_NAME,
    haider_tui::CRATE_NAME,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeProfile {
    /// A one-shot client only multiplexes RPC and its stdout adapter. Tool
    /// concurrency lives in `haiderd`, which retains its full runtime.
    EphemeralHeadless,
    /// The TUI keeps two async workers so terminal input/render and the daemon
    /// link remain independently schedulable without scaling idle client
    /// threads to host CPU count.
    Full,
}

impl RuntimeProfile {
    fn for_args(args: &[String]) -> Self {
        match args.first().map(String::as_str) {
            Some(
                "run" | "status" | "sessions" | "session" | "fleet" | "events" | "daemon"
                | "--ready",
            ) => Self::EphemeralHeadless,
            Some("resume") if args.iter().any(|argument| argument == "--json") => {
                Self::EphemeralHeadless
            }
            _ => Self::Full,
        }
    }

    fn build(self) -> io::Result<tokio::runtime::Runtime> {
        let mut builder = match self {
            Self::EphemeralHeadless => tokio::runtime::Builder::new_current_thread(),
            Self::Full => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(2);
                builder
            }
        };
        builder.enable_all().build()
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    run_cli(std::env::args().skip(1).collect())
}

// The CLI dispatcher composes several large async command futures. Windows
// gives the executable's main thread a much smaller stack than Unix, so poll
// it on an explicitly sized thread. Runtime selection inside that thread is
// otherwise identical on every platform.
#[cfg(windows)]
fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect();
    let launched = std::thread::Builder::new()
        .name("haider-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| run_cli(args));
    let code = match launched {
        Ok(thread) => match thread.join() {
            Ok(code) => code,
            Err(_) => {
                eprintln!("haider: main runtime thread panicked");
                ExitCode::from(EX_SOFTWARE)
            }
        },
        Err(error) => {
            eprintln!("haider: could not start main runtime thread: {error}");
            ExitCode::from(EX_SOFTWARE)
        }
    };
    hold_explorer_console_on_failure(code);
    code
}

/// Explorer destroys a double-clicked console as soon as the process exits.
/// Preserve the already-printed failure until a key is pressed, but only when
/// Win32 proves this process is alone, every standard stream is still attached
/// to the console, and no CI marker makes the invocation non-interactive.
#[cfg(windows)]
fn hold_explorer_console_on_failure(code: ExitCode) {
    use std::io::IsTerminal as _;

    if code == ExitCode::SUCCESS
        || std::env::var_os("CI").is_some()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
    {
        return;
    }
    let Ok(Some(console)) = haider_platform::sole_process_console() else {
        return;
    };
    eprintln!("haider: press any key to close this window");
    let _ = io::stderr().flush();
    let _ = console.wait_for_keypress();
}

/// Select the runtime before constructing an async command future. In
/// particular, one-shot `haider run` and `haider status` commands never
/// construct the omnibus dispatcher/TUI future or initialize terminal state.
fn run_cli(args: Vec<String>) -> ExitCode {
    let profile = RuntimeProfile::for_args(&args);
    let runtime = match profile.build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("haider: could not start async runtime: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    match profile {
        RuntimeProfile::EphemeralHeadless => match args.first().map(String::as_str) {
            Some("run") => {
                let rest = args.get(1..).unwrap_or_default();
                let code = runtime.block_on(run::run_command(rest));
                footprint_probe_hold();
                code
            }
            Some("status") => {
                let rest = args.get(1..).unwrap_or_default();
                let code = runtime.block_on(observe::status_command(rest));
                footprint_probe_hold();
                runtime.shutdown_background();
                code
            }
            Some("sessions") if args.get(1).is_some_and(|argument| argument == "wait-ready") => {
                let rest = args.get(2..).unwrap_or_default();
                let code = runtime.block_on(automation::sessions_wait_ready_command(rest));
                footprint_probe_hold();
                runtime.shutdown_background();
                code
            }
            Some("resume") => {
                let rest = args.get(1..).unwrap_or_default();
                let code = runtime.block_on(automation::resume_command(rest));
                footprint_probe_hold();
                runtime.shutdown_background();
                code
            }
            // The remaining commands selected for this profile are wire or
            // control-plane dispatchers. Preserve their existing semantics
            // while avoiding a host-sized worker pool.
            _ => {
                let code = runtime.block_on(dispatch(&args));
                footprint_probe_hold();
                runtime.shutdown_background();
                code
            }
        },
        RuntimeProfile::Full => {
            let code = runtime.block_on(dispatch(&args));
            footprint_probe_hold();
            code
        }
    }
}

/// Keep a completed client process alive for the release footprint harness.
///
/// This is deliberately an opt-in measurement seam rather than a command-line
/// surface: ordinary invocations never read a config file, allocate a timer,
/// or delay exit. The strict bound prevents a stale CI variable from parking a
/// client indefinitely.
fn footprint_probe_hold() {
    let Some(duration) = std::env::var_os(FOOTPRINT_HOLD_ENV)
        .and_then(|value| value.into_string().ok())
        .and_then(|value| footprint_hold_ms(&value))
    else {
        return;
    };
    std::thread::sleep(std::time::Duration::from_millis(duration));
}

fn footprint_hold_ms(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|duration| *duration > 0 && *duration <= MAX_FOOTPRINT_HOLD_MS)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod runtime_tests {
    use super::{
        AutoHermeticLookup, RuntimeProfile, StartupAutoHermeticTarget, auto_hermetic_status,
        auto_hermetic_status_for_startup_target, footprint_hold_ms, startup_auto_hermetic_target,
        suppress_automatic_update,
    };
    use tokio::runtime::RuntimeFlavor;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn runtime_flavor(profile: RuntimeProfile) -> RuntimeFlavor {
        let runtime = profile.build().expect("runtime builds");
        runtime.block_on(async { tokio::runtime::Handle::current().runtime_flavor() })
    }

    #[test]
    fn one_shot_commands_use_the_lean_current_thread_runtime() {
        for arguments in [
            args(&["run", "hello"]),
            args(&["status", "--json"]),
            args(&["sessions", "wait-ready", "--count", "1", "--json"]),
            args(&["sessions", "--json"]),
            args(&["session", "session-a", "--json"]),
            args(&["fleet", "--json"]),
            args(&["events"]),
            args(&["daemon", "stop", "--json"]),
            args(&["--ready"]),
            args(&["resume", "session-a", "--json"]),
        ] {
            let profile = RuntimeProfile::for_args(&arguments);
            assert_eq!(profile, RuntimeProfile::EphemeralHeadless);
            assert_eq!(runtime_flavor(profile), RuntimeFlavor::CurrentThread);
        }
    }

    #[test]
    fn tui_and_non_wire_paths_keep_the_full_runtime() {
        for arguments in [
            args(&[]),
            args(&["tui"]),
            args(&["resume"]),
            args(&["resume", "session-a"]),
            args(&["self-test"]),
            args(&["account", "list", "--json"]),
            args(&["update", "--check"]),
            args(&["--version"]),
        ] {
            assert_eq!(RuntimeProfile::for_args(&arguments), RuntimeProfile::Full);
        }
        assert_eq!(
            runtime_flavor(RuntimeProfile::Full),
            RuntimeFlavor::MultiThread
        );
        assert_eq!(
            RuntimeProfile::Full
                .build()
                .expect("TUI runtime builds")
                .metrics()
                .num_workers(),
            2
        );
    }

    #[test]
    fn footprint_hold_is_opt_in_and_bounded() {
        assert_eq!(footprint_hold_ms("60000"), Some(60_000));
        assert_eq!(footprint_hold_ms("0"), None);
        assert_eq!(footprint_hold_ms("300001"), None);
        assert_eq!(footprint_hold_ms("not-a-duration"), None);
    }

    #[test]
    fn only_auto_hermetic_lockdown_suppresses_the_automatic_update() {
        let mut status = haider_rpc::LockdownStatusWire {
            provider: Some("local".into()),
            activation: Some(haider_rpc::LockdownActivationWire::AutoHermetic),
            reason: Some("active local no-auth provider".into()),
            tools_allowed: vec!["fs_read".into()],
            quota_used: 0,
            quota_limit: 1,
        };
        assert_eq!(
            auto_hermetic_status(Some(&status)),
            AutoHermeticLookup::Active
        );
        status.activation = Some(haider_rpc::LockdownActivationWire::AutoHermeticEligible);
        assert_eq!(
            auto_hermetic_status(Some(&status)),
            AutoHermeticLookup::Active,
            "an imminent new session treats prospective eligibility as applicable"
        );
        status.activation = Some(haider_rpc::LockdownActivationWire::Configured);
        assert_eq!(
            auto_hermetic_status(Some(&status)),
            AutoHermeticLookup::Inactive
        );
        assert_eq!(auto_hermetic_status(None), AutoHermeticLookup::Unavailable);
        assert!(suppress_automatic_update(AutoHermeticLookup::Active));
        assert!(!suppress_automatic_update(AutoHermeticLookup::Inactive));
        assert!(
            suppress_automatic_update(AutoHermeticLookup::Unavailable),
            "an unavailable policy lookup must fail closed before network discovery"
        );
    }

    #[test]
    fn startup_update_policy_checks_only_the_provider_this_door_activates() {
        assert_eq!(
            startup_auto_hermetic_target(Some("session-a"), Some("default-provider")),
            StartupAutoHermeticTarget::Session("session-a")
        );
        assert_eq!(
            startup_auto_hermetic_target(None, Some("local-default")),
            StartupAutoHermeticTarget::Provider("local-default")
        );
        assert_eq!(
            startup_auto_hermetic_target(None, None),
            StartupAutoHermeticTarget::None
        );
        assert_eq!(
            auto_hermetic_status_for_startup_target(StartupAutoHermeticTarget::None, None),
            AutoHermeticLookup::Unavailable,
            "the session picker must not arm egress before its active provider is known"
        );
    }
}

async fn dispatch(args: &[String]) -> ExitCode {
    match parse_bare_tui_options(args) {
        Ok(Some(options)) => return front_door_with_options(FrontDoor::Tui, options).await,
        Ok(None) => {}
        Err(message) => {
            eprintln!("haider: {message}");
            return ExitCode::from(2);
        }
    }
    match args {
        [version] if matches!(version.as_str(), "--version" | "-V" | "version") => {
            println!("haider {VERSION}");
            ExitCode::SUCCESS
        }
        [command] if command == "self-test" => self_test().await,
        // Non-interactive readiness: connect-or-spawn, report, exit,
        // leaving the daemon running. The scriptable half of the front
        // door (bare `haider` on a TTY enters the live TUI instead).
        [command] if command == "--ready" => front_door(FrontDoor::Report).await,
        [command, rest @ ..] if command == "run" => run::run_command(rest).await,
        [command, rest @ ..] if command == "status" => observe::status_command(rest).await,
        [command, subcommand, rest @ ..] if command == "sessions" && subcommand == "wait-ready" => {
            automation::sessions_wait_ready_command(rest).await
        }
        [command, rest @ ..] if command == "daemon" => daemon::daemon_command(rest).await,
        [command, rest @ ..] if command == "sessions" => observe::sessions_command(rest).await,
        [command, session_id, subcommand, rest @ ..]
            if command == "session" && subcommand == "config" =>
        {
            session_config::session_config_command(session_id, rest).await
        }
        [command, session_id, subcommand, rest @ ..]
            if command == "session" && subcommand == "recover" =>
        {
            session_recover::session_recover_command(session_id, rest).await
        }
        [command, session_id, subcommand, rest @ ..]
            if command == "session" && subcommand == "seen" =>
        {
            session_seen::session_seen_command(session_id, rest).await
        }
        [command, subcommand, rest @ ..] if command == "session" && subcommand == "workspace" => {
            session_workspace::session_workspace_command(None, rest).await
        }
        [command, session_id, subcommand, rest @ ..]
            if command == "session" && subcommand == "workspace" =>
        {
            session_workspace::session_workspace_command(Some(session_id), rest).await
        }
        [command, session_id, subcommand, seq, rest @ ..]
            if command == "session" && subcommand == "item" =>
        {
            session_item::session_item_command(session_id, seq, rest).await
        }
        [command, rest @ ..] if command == "session" => observe::session_command(rest).await,
        [command, rest @ ..] if command == "account" => account::account_command(rest).await,
        [command, rest @ ..] if command == "provider" => provider::provider_command(rest).await,
        [command, rest @ ..] if command == "lockdown" => lockdown::lockdown_command(rest).await,
        [command, rest @ ..] if command == "models" => models::models_command(rest).await,
        [command, rest @ ..] if command == "fleet" => observe::fleet_command(rest).await,
        [command, rest @ ..] if command == "events" => observe::events_command(rest).await,
        [command, rest @ ..] if command == "graph" => graph::graph_command(rest).await,
        [command, rest @ ..] if command == "export" => export::export_command(rest).await,
        [command, rest @ ..] if command == "hooks" => hooks::hooks_command(rest).await,
        [command, rest @ ..] if command == "peer" => peer::peer_command(rest).await,
        [command, rest @ ..] if command == "ssh" => ssh::ssh_command(rest).await,
        [command, rest @ ..] if command == "shell" => shell_registry::shell_command(rest).await,
        [command, rest @ ..] if command == "update" => update::update_command(rest).await,
        [command, rest @ ..] if command == "tui" => tui_command(rest).await,
        // Owner 2026-08-21: `haider resume` opens the all-sessions picker;
        // `haider resume <id>` attaches that session directly.
        [command, rest @ ..]
            if command == "resume" && rest.iter().any(|argument| argument == "--json") =>
        {
            automation::resume_command(rest).await
        }
        [command, rest @ ..] if command == "resume" => match rest {
            [] => {
                front_door_with_options(
                    FrontDoor::Tui,
                    BareTuiOptions {
                        browse_sessions: true,
                        ..BareTuiOptions::default()
                    },
                )
                .await
            }
            [id] if !id.is_empty() && !id.starts_with('-') => {
                front_door_with_options(
                    FrontDoor::Tui,
                    BareTuiOptions {
                        session: Some(id.clone()),
                        ..BareTuiOptions::default()
                    },
                )
                .await
            }
            _ => {
                eprintln!("usage: haider resume [<session-id>]");
                ExitCode::from(2)
            }
        },
        [command, rest @ ..] if command == "import" => import_command(rest).await,
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run (-p <prompt>|-|<prompt>) \
                 [--json|--output print|json|jsonl] [--timeout <dur>] \
                 [--max-tokens <n>] [--max-cost <usd>] [--max-time <dur>] [--seed <n>] \
                 [--start] | run --status <run-id> | run --stop <run-id> | run --replay <run-id> \
                 [--model <model|provider/model>] [--effort <level>] [--speed <fast|normal>] [--account <alias>] \
                 [--read-only] [--allow-writes] [--allow-exec] [--auto-allow] [--trust-hooks] [--attach <path>]..., \
                 status [--json] [--no-spawn], daemon stop [--json] [--timeout <duration>], \
                 sessions [--recovery] [--json] [--no-spawn], \
                 sessions wait-ready --count <n> [--session <id>]... [--timeout <dur>] --json [--no-spawn], \
                 resume [<session-id>], resume <session-id> --json [--timeout <dur>] [--no-spawn], \
                 session <id> [--json|--watch] [--no-spawn], \
                 session <id> config [--json] [--model <model|provider/model>] [--effort <level>] [--speed <fast|normal>] [--account <alias>], \
                 session <id> seen, session <id> recover [--json] [--probe|--mark-done|--retry|--abandon], \
                 session workspace set <path>, session <id> workspace set <path>, \
                 session <id> item <seq> --json [--masked] [--no-spawn], \
                 account list [--json], account use <alias> [--confirm], account source list [--json], account source add <codex|claude_file|grok|kimi_code_home> <root> [--label <label>], account source remove <source-id>, account source scan [--json], account import <codex|claude-code> [--confirm], account refresh <alias>, account remove <alias> --confirm, \
                 account add <alias> --base-url <url> [--api-key <key>|--api-key-env <VAR>|--api-key-stdin|--no-auth] [--api-family openai|anthropic] [--response-open-timeout <dur>] [--chunk-idle-timeout <dur>] [--semantic-progress-timeout <dur>] [--json], \
                 account probe <alias> [--json], account update <alias> [--base-url <url>] [--api-key <key>|--api-key-env <VAR>|--api-key-stdin] [--response-open-timeout <dur>] [--chunk-idle-timeout <dur>] [--semantic-progress-timeout <dur>] [--json], \
                 provider list [--json], provider show <name> [--json], provider add <name> --base-url <url> [--lockdown|--full] ..., provider set <name> (--lockdown|--full), provider remove <name> --confirm, \
                 lockdown status [--json], lockdown quota [--set <bytes>] [--json], \
                 models [--json] [--refresh [<alias>]], \
                 fleet [<session-id>] [--json] [--no-spawn], \
                 events [--follow] [--no-spawn], \
                 graph status <session-id> [--json], graph pin <session-id>, \
                 graph abandon <session-id> [why], \
                 export <session-id> [--format markdown|json|codex|claude-code|opencode|pipe] [--out PATH] [--masked] [--confirm], \
                 hooks list [--json], hooks trust <digest>, hooks revoke <digest>, \
                 peer list [--json], peer send <name> <message|->, peer name <new-name>, peer watch, \
                 update [--check], \
                 tui [--theme system|light|dark|desert|oasis] [--session <id>] [--no-update-check], tui --demo [--plain], \
                 import [codex|claude-code], [--session <id>] [--no-update-check], --ready)"
            );
            ExitCode::from(2)
        }
        // Bare-TUI arguments were handled before command dispatch.
        [] => unreachable!("bare TUI dispatch is handled above"),
    }
}

/// Additive options accepted by the bare `haider` live-TUI front door.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BareTuiOptions {
    pub session: Option<String>,
    pub no_update_check: bool,
    /// `haider resume` / `--resume` with no id: boot straight into the
    /// all-sessions browser so the user PICKS a session (owner 2026-08-21).
    pub browse_sessions: bool,
}

/// Parse only the bare-TUI argument vocabulary. `Ok(None)` leaves ordinary
/// subcommands to the main dispatcher.
pub(crate) fn parse_bare_tui_options(args: &[String]) -> Result<Option<BareTuiOptions>, String> {
    if args.is_empty() {
        return Ok(Some(BareTuiOptions::default()));
    }
    if !matches!(
        args[0].as_str(),
        "--session" | "--no-update-check" | "--resume"
    ) {
        return Ok(None);
    }
    let mut options = BareTuiOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-update-check" if !options.no_update_check => options.no_update_check = true,
            "--no-update-check" => return Err("--no-update-check was supplied twice".into()),
            "--session" if options.session.is_none() => {
                let id = iter
                    .next()
                    .filter(|id| !id.is_empty() && !id.starts_with('-'))
                    .ok_or_else(|| "--session requires a session id".to_owned())?;
                options.session = Some(id.clone());
            }
            "--session" => return Err("--session was supplied twice".into()),
            // `--resume` alone opens the picker; `--resume <id>` is the
            // same door as `--session <id>` (attach that one directly).
            "--resume" if !options.browse_sessions && options.session.is_none() => {
                match iter.clone().next() {
                    Some(id) if !id.is_empty() && !id.starts_with('-') => {
                        options.session = Some(id.clone());
                        iter.next();
                    }
                    _ => options.browse_sessions = true,
                }
            }
            "--resume" => return Err("--resume was supplied twice".into()),
            other => return Err(format!("unknown bare-TUI flag `{other}`")),
        }
    }
    Ok(Some(options))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportDispatch {
    List,
    Source(ImportSource),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportSource {
    Codex,
    ClaudeCode,
}

impl ImportSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
        }
    }
}

pub(crate) fn parse_import_dispatch(rest: &[String]) -> Result<ImportDispatch, String> {
    match rest {
        [] => Ok(ImportDispatch::List),
        [source] if source == "codex" => Ok(ImportDispatch::Source(ImportSource::Codex)),
        [source] if source == "claude-code" => Ok(ImportDispatch::Source(ImportSource::ClaudeCode)),
        [source] => Err(format!(
            "unknown source `{source}` (expected codex or claude-code)"
        )),
        _ => Err("expected at most one source: codex or claude-code".into()),
    }
}

async fn import_command(rest: &[String]) -> ExitCode {
    let dispatch = match parse_import_dispatch(rest) {
        Ok(dispatch) => dispatch,
        Err(message) => {
            eprintln!("haider import: {message}");
            return ExitCode::from(2);
        }
    };
    match dispatch {
        ImportDispatch::List => {
            println!("transcript sources: codex, claude-code");
            println!("credential adoption: haider account import <source> --confirm");
            ExitCode::SUCCESS
        }
        ImportDispatch::Source(source) => {
            eprintln!(
                "haider import {} is the transcript-import namespace; credential adoption requires `haider account import {} --confirm`",
                source.as_str(),
                source.as_str()
            );
            ExitCode::from(2)
        }
    }
}

/// What the front door does once the daemon is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontDoor {
    /// Enter the live TUI on the connection (bare `haider`, `haider tui`).
    Tui,
    /// Report readiness and exit, leaving the daemon running
    /// (`haider --ready`, and the non-interactive fallback).
    Report,
}

/// Bare `haider`: resolve the shared profile, connect-or-spawn the daemon,
/// verify features, and enter the live TUI. The daemon outlives us either
/// way — closing this connection never implies daemon shutdown (R8).
async fn front_door(mode: FrontDoor) -> ExitCode {
    front_door_with_options(mode, BareTuiOptions::default()).await
}

/// ADE seam: optional `--session <id>` opens attached once the daemon's list
/// proves it exists; the update policy bit controls only automatic checks.
async fn front_door_with_options(mode: FrontDoor, options: BareTuiOptions) -> ExitCode {
    let BareTuiOptions {
        session: initial_session,
        no_update_check,
        browse_sessions,
    } = options;
    let env = haider_client::ProfileEnv::capture();
    let profile = match haider_client::resolve_profile(&env) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let interactive = mode == FrontDoor::Tui && std::io::IsTerminal::is_terminal(&io::stdout());
    let mut ensure_options = haider_client::EnsureOptions::default();
    if !interactive {
        ensure_options
            .required_features
            .insert(haider_rpc::FEATURE_STATUS_SNAPSHOT_V1.to_owned());
        ensure_options
            .required_features
            .insert(haider_rpc::FEATURE_STATUS_RUNTIME_V1.to_owned());
    }
    match haider_client::ensure_daemon(&profile, ensure_options).await {
        Ok(ensured) => {
            let how = match (ensured.spawned, ensured.race_lost) {
                (false, _) => "already running".to_owned(),
                (true, false) => "spawned".to_owned(),
                (true, true) => format!(
                    "spawned; our candidate lost the startup race (exit {}) and we attached to \
                     the winner",
                    haider_client::RACE_LOSER_EXIT_CODE
                ),
            };
            if !interactive {
                let positive_readiness = match ensured
                    .client
                    .request(haider_rpc::RequestBody::StatusSnapshot {})
                    .await
                {
                    Ok(haider_rpc::ResponseBody::StatusSnapshot {
                        ready: true,
                        ready_since: Some(_),
                        providers_loaded: true,
                        ..
                    }) => true,
                    Ok(haider_rpc::ResponseBody::StatusSnapshot { .. }) => false,
                    Ok(_) => false,
                    Err(error) => {
                        eprintln!("haider: daemon readiness status failed: {error}");
                        false
                    }
                };
                if !positive_readiness {
                    eprintln!(
                        "haider: daemon did not report positive readiness (store, recovery, provider registry, and session hub)"
                    );
                    let _ = ensured.client.close();
                    return ExitCode::from(EX_UNAVAILABLE);
                }
                println!(
                    "haider {VERSION} — daemon ready ({how}): profile {} at {} \
                     (daemon v{}, generation {})",
                    &profile.profile_id[..12],
                    profile.endpoint_path.display(),
                    ensured.welcome.daemon_version,
                    ensured.welcome.daemon_generation,
                );
                // Parent exit leaves the daemon running (R8 shutdown
                // policy): closing this connection never implies shutdown.
                let _ = ensured.client.close();
                return ExitCode::SUCCESS;
            }
            let new_session_provider =
                (!browse_sessions).then_some(profile.default_provider.as_str());
            let auto_hermetic = startup_auto_hermetic(
                &ensured.client,
                initial_session.as_deref(),
                new_session_provider,
            )
            .await;
            let suppress_update = suppress_automatic_update(auto_hermetic);
            if matches!(auto_hermetic, AutoHermeticLookup::Unavailable) {
                eprintln!(
                    "haider: automatic update check suppressed because the active provider's \
                     hermetic policy could not be established"
                );
            }
            let mut model = live_model(&profile);
            model.initial_session = initial_session.map(haider_protocol::ids::SessionId::new);
            if browse_sessions {
                model.enter_sessions();
            }
            let updates = update::tui::live_update_bridge(
                profile.store_dir.clone(),
                no_update_check || suppress_update,
            );
            match run_live(
                model,
                ensured.client,
                profile,
                live_client_config(),
                updates,
            )
            .await
            {
                Ok(LiveExit::Quit) => ExitCode::SUCCESS,
                Ok(LiveExit::UpdateInstalled) => {
                    let executable = match std::env::current_exe() {
                        Ok(executable) => executable,
                        Err(error) => {
                            eprintln!("haider: cannot resolve updated executable: {error}");
                            return ExitCode::from(EX_IOERR);
                        }
                    };
                    let argv: Vec<_> = std::env::args_os().collect();
                    let plan = update::tui_restart::restart_plan(executable, &argv);
                    match update::tui_restart::execute_restart(plan) {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(error) => {
                            eprintln!("haider: cannot restart updated TUI: {error}");
                            ExitCode::from(EX_IOERR)
                        }
                    }
                }
                Err(error) => {
                    eprintln!("haider: terminal error: {error}");
                    ExitCode::from(EX_IOERR)
                }
            }
        }
        Err(error) => {
            eprintln!("haider: {error}");
            ExitCode::from(front_door_exit_code(&error))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoHermeticLookup {
    Active,
    Inactive,
    Unavailable,
}

fn auto_hermetic_status(status: Option<&haider_rpc::LockdownStatusWire>) -> AutoHermeticLookup {
    match status.map(|status| status.activation) {
        Some(Some(
            haider_rpc::LockdownActivationWire::AutoHermetic
            | haider_rpc::LockdownActivationWire::AutoHermeticEligible,
        )) => AutoHermeticLookup::Active,
        Some(_) => AutoHermeticLookup::Inactive,
        None => AutoHermeticLookup::Unavailable,
    }
}

const fn suppress_automatic_update(lookup: AutoHermeticLookup) -> bool {
    !matches!(lookup, AutoHermeticLookup::Inactive)
}

async fn provider_auto_hermetic(
    client: &haider_client::RpcClient,
    provider: &str,
) -> AutoHermeticLookup {
    match client
        .request(haider_rpc::RequestBody::LockdownStatus {
            provider: Some(provider.to_owned()),
        })
        .await
    {
        Ok(haider_rpc::ResponseBody::LockdownStatus { status }) => {
            auto_hermetic_status(Some(&status))
        }
        _ => AutoHermeticLookup::Unavailable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAutoHermeticTarget<'a> {
    Session(&'a str),
    Provider(&'a str),
    None,
}

fn auto_hermetic_status_for_startup_target(
    target: StartupAutoHermeticTarget<'_>,
    status: Option<&haider_rpc::LockdownStatusWire>,
) -> AutoHermeticLookup {
    match target {
        StartupAutoHermeticTarget::Session(_) | StartupAutoHermeticTarget::Provider(_) => {
            auto_hermetic_status(status)
        }
        StartupAutoHermeticTarget::None => AutoHermeticLookup::Unavailable,
    }
}

fn startup_auto_hermetic_target<'a>(
    initial_session: Option<&'a str>,
    new_session_provider: Option<&'a str>,
) -> StartupAutoHermeticTarget<'a> {
    if let Some(session) = initial_session {
        StartupAutoHermeticTarget::Session(session)
    } else if let Some(provider) = new_session_provider {
        StartupAutoHermeticTarget::Provider(provider)
    } else {
        StartupAutoHermeticTarget::None
    }
}

/// Reads only the provider that this front door will actually open before
/// arming the client-side startup update check. An explicit session's frozen
/// observation wins. A new-session door evaluates the profile's default
/// provider; the session picker passes no provider because it has no active
/// selection yet.
async fn startup_auto_hermetic(
    client: &haider_client::RpcClient,
    initial_session: Option<&str>,
    new_session_provider: Option<&str>,
) -> AutoHermeticLookup {
    let target = startup_auto_hermetic_target(initial_session, new_session_provider);
    match target {
        StartupAutoHermeticTarget::Session(initial_session) => {
            let response = client
                .request(haider_rpc::RequestBody::SessionObserve {
                    session_id: SessionId::new(initial_session),
                    last_event_limit: 0,
                    metadata_only: true,
                })
                .await;
            match response {
                Ok(haider_rpc::ResponseBody::SessionObserve { digest }) => {
                    auto_hermetic_status_for_startup_target(target, digest.lockdown.as_ref())
                }
                _ => AutoHermeticLookup::Unavailable,
            }
        }
        StartupAutoHermeticTarget::Provider(provider) => {
            provider_auto_hermetic(client, provider).await
        }
        StartupAutoHermeticTarget::None => auto_hermetic_status_for_startup_target(target, None),
    }
}

/// sysexits mapping for front-door failures: 76 `EX_PROTOCOL` for wire/skew
/// diagnostics, 69 `EX_UNAVAILABLE` for a daemon that never became ready,
/// 74 `EX_IOERR` for transport faults, 70 otherwise.
fn front_door_exit_code(error: &haider_client::EnsureError) -> u8 {
    use haider_client::EnsureError;
    match error {
        EnsureError::ProtocolMismatch(_)
        | EnsureError::MissingFeatures { .. }
        | EnsureError::ProfileMismatch { .. } => 76,
        EnsureError::DaemonExited { .. } | EnsureError::StartupTimeout { .. } => 69,
        EnsureError::Connect(_) => EX_IOERR,
        EnsureError::Spawn { .. } => EX_SOFTWARE,
    }
}

/// `haider tui --demo [--plain] [--theme system|light|dark|desert|oasis]`
/// — the scripted
/// demo drives every surface until the daemon lands (W3). `--plain` (or a
/// non-TTY stdout, research rec 2) renders the final state as plain text
/// instead of taking the terminal. `HAIDER_SHAHADA=translit` selects the
/// transliteration sanctum tier.
async fn tui_command(rest: &[String]) -> ExitCode {
    use std::io::IsTerminal;
    let mut demo = false;
    let mut plain = false;
    let mut theme: Option<ThemeChoice> = None;
    let mut session: Option<String> = None;
    let mut no_update_check = false;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--plain" => plain = true,
            "--no-update-check" if !no_update_check => no_update_check = true,
            "--no-update-check" => {
                eprintln!("haider tui: --no-update-check was supplied twice");
                return ExitCode::from(2);
            }
            "--session" => match iter
                .next()
                .filter(|id| !id.is_empty() && !id.starts_with('-'))
            {
                Some(id) => session = Some(id.clone()),
                None => {
                    eprintln!("haider tui: --session requires a session id");
                    return ExitCode::from(2);
                }
            },
            "--theme" => match iter.next().and_then(|name| ThemeChoice::parse(name)) {
                Some(key) => theme = Some(key),
                None => {
                    eprintln!("haider tui: --theme takes system|light|dark|desert|oasis");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("haider tui: unknown flag `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    if !demo {
        // W3c3: `haider tui` is the LIVE TUI, exactly like bare `haider`.
        // `--theme` still applies; `--plain` has no live meaning (there is
        // no deterministic oracle for a live daemon) and is rejected rather
        // than silently ignored.
        if plain {
            eprintln!("haider tui: --plain is a demo-only oracle; use `--demo --plain`");
            return ExitCode::from(2);
        }
        return front_door_with_options(
            FrontDoor::Tui,
            BareTuiOptions {
                session,
                no_update_check,
                browse_sessions: false,
            },
        )
        .await;
    }
    if no_update_check {
        eprintln!("haider tui: --no-update-check is live-only; drop --demo");
        return ExitCode::from(2);
    }
    if session.is_some() {
        // Demo sessions are fabricated locally — a daemon session id has
        // no meaning there; reject rather than silently ignore.
        eprintln!("haider tui: --session is live-only; drop --demo");
        return ExitCode::from(2);
    }
    let interactive = !plain && io::stdout().is_terminal();
    let mut model = AppModel::new();
    // Arabic is the default sanctum tier (owner decision, sim parity);
    // HAIDER_SHAHADA=translit serves emulators that cannot shape Arabic.
    if matches!(std::env::var("HAIDER_SHAHADA").as_deref(), Ok("translit")) {
        model.sanctum_tier = SanctumTier::Translit;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let home = std::env::var("HOME").unwrap_or_default();
        // Component-aware: /Users/alice2 must not abbreviate under ~alice.
        // Seeds the launcher + session working dirs (TUI3b: the shell
        // builtins' `cd` retargets these; unknown dirs list VFS defaults).
        let abbreviated = match (!home.is_empty())
            .then(|| cwd.strip_prefix(&home).ok())
            .flatten()
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Some(rest) => format!("~/{}", rest.display()),
            None => cwd.display().to_string(),
        };
        model.launcher_dir = abbreviated.clone();
        model.session_dir = abbreviated;
    }
    if !interactive {
        // The plain/CI oracle stays deterministic: no demo-store load, no
        // save, no terminal probe — `system` resolves to the dark default.
        model.apply_theme_choice(theme.unwrap_or_default());
        // Fallible write: `print!` panics on BrokenPipe (review r1 P2).
        // A closed pipe is a normal consumer choice → success; other write
        // failures are real I/O errors.
        let text = run_demo_plain(model);
        let mut out = io::stdout();
        return match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(EX_IOERR),
        };
    }
    // TUI4c-13b: the DEMO state file (sim localStorage) — load before the
    // theme decision so a pre-wave file's theme name can still migrate.
    // A missing/corrupt file or an unresolvable path simply keeps the
    // seeds; the demo never fails to start over persistence.
    let mut legacy_theme = None;
    let store = DemoStore::default_path().map(|path| {
        let store = DemoStore::at(path);
        if let Some(dto) = store.load() {
            legacy_theme = haider_tui::demo_store::hydrate(&mut model, dto).legacy_theme;
        }
        store
    });
    // Theme CHOICE precedence (owner spec §3): explicit --theme, then the
    // profile-dir settings file, then a pre-wave demo file's theme name
    // (one-shot migration), then `system` — which resolves against the
    // detected terminal appearance (OSC 11 / COLORFGBG, undetectable →
    // dark), re-evaluated on every boot.
    model.detected_system = detect_system_theme();
    let settings_choice = SettingsStore::open_default().and_then(|store| store.load());
    model.apply_theme_choice(
        theme
            .or(settings_choice)
            .or(legacy_theme.map(ThemeChoice::Fixed))
            .unwrap_or_default(),
    );
    match run_demo(model, store).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("haider tui: terminal error: {error}");
            ExitCode::from(EX_IOERR)
        }
    }
}

/// The client identity live mode presents, and the source of the durable
/// command-id prefix (`{instance}-{n}`): random per process so two clients
/// can never mint the same idempotency key.
fn live_client_config() -> haider_client::ClientConfig {
    haider_client::ClientConfig {
        client_name: "haider-tui".to_owned(),
        client_instance_id: format!(
            "tui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ),
        ..haider_client::ClientConfig::default()
    }
}

/// A model for LIVE mode: no demo seeds, the real cwd, and the sanctum
/// tier. The session list arrives from the daemon (R11 cut 4: no locally
/// fabricated row exists), so the launcher starts empty and fills in.
fn live_model(profile: &haider_client::ResolvedProfile) -> AppModel {
    let mut model = AppModel::new();
    model.sessions.clear();
    model.mode = haider_tui::app::RuntimeMode::Live;
    // The RESOLVED PROFILE owns the defaults every new session is created
    // with — never a UI constant. `session.create` validates the provider
    // against the daemon's own whitelist, so a client that hardcoded one
    // would be rejected by the very daemon it just resolved.
    model.identity.provider = profile.default_provider.clone();
    model.identity.model_short = profile.default_model.clone();
    model.identity.context_window = profile.default_max_tokens;
    if matches!(std::env::var("HAIDER_SHAHADA").as_deref(), Ok("translit")) {
        model.sanctum_tier = SanctumTier::Translit;
    }
    apply_cwd(&mut model);
    // W-C M1: load custom slash commands from `.haider/commands` (project,
    // walked up from cwd) + `~/.haider/commands` (global). This is shell-owned
    // IO at construction — the reducer never touches disk; a malformed file is
    // skipped and surfaced, never fatal.
    let commands_cwd = std::env::current_dir().ok();
    let commands_home = std::env::var_os("HOME").map(PathBuf::from);
    let loaded =
        haider_tui::custom_commands::load_for(commands_cwd.as_deref(), commands_home.as_deref());
    model.set_custom_commands(loaded.commands, loaded.warnings);
    // Same choice ladder as the demo, minus the flag and the legacy demo
    // file: settings file, else `system` against the boot-time detection.
    model.detected_system = detect_system_theme();
    let choice = SettingsStore::open_default()
        .and_then(|store| store.load())
        .unwrap_or_default();
    model.apply_theme_choice(choice);
    model
}

/// Abbreviate the process cwd into the launcher/session dirs.
fn apply_cwd(model: &mut AppModel) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let abbreviated = match (!home.is_empty())
        .then(|| cwd.strip_prefix(&home).ok())
        .flatten()
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => cwd.display().to_string(),
    };
    model.launcher_dir = abbreviated.clone();
    model.session_dir = abbreviated;
    model.cwd = cwd.display().to_string();
}

fn cli_config(worker_generation: u64, model: &str) -> HarnessConfig {
    let mut config = HarnessConfig::for_session(
        SessionId::new("cli-session"),
        DeviceId::new("cli-device"),
        1,
        worker_generation,
    );
    config.model = model.into();
    config
}
/// Offline, ephemeral, deterministic. Structured JSON on stdout.
async fn self_test() -> ExitCode {
    let mut checks: Vec<serde_json::Value> = CRATES
        .iter()
        .map(|name| serde_json::json!({"name": format!("link:{name}"), "ok": true}))
        .collect();
    let fake_turn_ok = fake_turn_self_test().await;
    checks.push(serde_json::json!({
        "name": "fake-provider-turn",
        "ok": fake_turn_ok,
    }));
    let report = serde_json::json!({
        "schema": "haider.selftest.v0",
        "version": VERSION,
        "ok": fake_turn_ok,
        "checks": checks,
    });
    println!("{report}");
    if fake_turn_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn fake_turn_self_test() -> bool {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText { text: "ok".into() },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(cli_config(1, "fake-model"), provider, store.clone());
    let Ok(turn) = handle.submit_turn(SubmitTurn::new("self-test")).await else {
        return false;
    };
    let Ok(outcome) = turn.wait().await else {
        return false;
    };
    if outcome.state != RunState::Done || outcome.error.is_some() {
        return false;
    }
    StoreHandle::latest_seq(store.as_ref(), &SessionId::new("cli-session"))
        .await
        .is_ok_and(|sequence| sequence > 0)
}
