//! Headless/control client; interactive code lives in the sibling payload.
use std::process::ExitCode;
pub(crate) mod account;
pub(crate) mod agent;
pub(crate) mod automation;
pub(crate) mod daemon;
pub(crate) mod export;
pub(crate) mod graph;
pub(crate) mod hooks;
pub(crate) mod lockdown;
pub(crate) mod models;
pub(crate) mod observe;
pub(crate) mod provider;
pub(crate) mod run;
pub(crate) mod session_config;
pub(crate) mod session_item;
pub(crate) mod session_provider;
pub(crate) mod session_recover;
pub(crate) mod session_retract;
pub(crate) mod session_seen;
pub(crate) mod session_workspace;
pub(crate) mod shell_registry;
pub mod ssh;
pub mod update;

mod payload;
pub mod routing;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const EX_UNAVAILABLE: u8 = 69;
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;
const FOOTPRINT_HOLD_ENV: &str = "HAIDER_CLIENT_FOOTPRINT_HOLD_MS";
const MAX_FOOTPRINT_HOLD_MS: u64 = 5 * 60 * 1_000;

/// The Windows control dispatcher needs the established 8 MiB main stack.
/// The payload is selected before creating either this thread or Tokio.
pub fn main() -> ExitCode {
    let code = dispatch_main();
    #[cfg(windows)]
    hold_explorer_console_on_failure(code);
    code
}

fn dispatch_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match routing::parse_command(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("haider: {message}");
            return ExitCode::from(2);
        }
    };
    match command {
        routing::Command::Version => {
            println!("haider {VERSION}");
            return ExitCode::SUCCESS;
        }
        routing::Command::SelfTest => return payload::self_test(),
        routing::Command::InstallBundle(source, destination) => {
            return match update::install_bundle_from_directory(
                std::path::Path::new(source),
                std::path::Path::new(destination),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("haider install: {error}");
                    ExitCode::from(error.exit_code())
                }
            };
        }
        routing::Command::Interactive(_) => return payload::launch(),
        _ => {}
    }
    #[cfg(not(windows))]
    {
        run_headless(command)
    }
    #[cfg(windows)]
    {
        let launched = std::thread::Builder::new()
            .name("haider-main".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || match routing::parse_command(&args) {
                Ok(command) => run_headless(command),
                Err(message) => {
                    eprintln!("haider: {message}");
                    ExitCode::from(2)
                }
            });
        match launched {
            Ok(thread) => thread.join().unwrap_or_else(|_| {
                eprintln!("haider: main runtime thread panicked");
                ExitCode::from(EX_SOFTWARE)
            }),
            Err(error) => {
                eprintln!("haider: could not start main runtime thread: {error}");
                ExitCode::from(EX_SOFTWARE)
            }
        }
    }
}

fn run_headless(command: routing::Command<'_>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("haider: could not start async runtime: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let is_run = matches!(command, routing::Command::Run(_));
    let code = runtime.block_on(dispatch(command));
    footprint_probe_hold();
    // Retain run's existing owned output/daemon teardown before runtime drop.
    if !is_run {
        runtime.shutdown_background();
    }
    code
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

async fn dispatch(command: routing::Command<'_>) -> ExitCode {
    use routing::Command;
    match command {
        Command::Run(rest) => run::run_command(rest).await,
        Command::Status(rest) => observe::status_command(rest).await,
        Command::SessionsWaitReady(rest) => automation::sessions_wait_ready_command(rest).await,
        Command::Daemon(rest) => daemon::daemon_command(rest).await,
        Command::Sessions(rest) => observe::sessions_command(rest).await,
        Command::SessionProvider(rest) => session_provider::command(rest).await,
        Command::SessionRetract(rest) => session_retract::command(rest).await,
        Command::Session(rest) => observe::session_command(rest).await,
        Command::SessionSubmit(rest) => run::session_submit_command(rest).await,
        Command::Account(rest) => account::account_command(rest).await,
        Command::Provider(rest) => provider::provider_command(rest).await,
        Command::Lockdown(rest) => lockdown::lockdown_command(rest).await,
        Command::Models(rest) => models::models_command(rest).await,
        Command::Fleet(rest) => observe::fleet_command(rest).await,
        Command::Events(rest) => observe::events_command(rest).await,
        Command::Graph(rest) => graph::graph_command(rest).await,
        Command::Export(rest) => export::export_command(rest).await,
        Command::Hooks(rest) => hooks::hooks_command(rest).await,
        Command::Ssh(rest) => ssh::ssh_command(rest).await,
        Command::Shell(rest) => shell_registry::shell_command(rest).await,
        Command::Update(rest) => update::update_command(rest).await,
        Command::ResumeJson(rest) => automation::resume_command(rest).await,
        Command::Import(rest) => import_command(rest).await,
        Command::Agent(family, rest) => agent::command(family, rest).await,
        Command::SessionConfig(id, rest) => session_config::session_config_command(id, rest).await,
        Command::SessionRecover(id, rest) => {
            session_recover::session_recover_command(id, rest).await
        }
        Command::SessionSeen(id, rest) => session_seen::session_seen_command(id, rest).await,
        Command::SessionWorkspace(id, rest) => {
            session_workspace::session_workspace_command(id, rest).await
        }
        Command::SessionItem(id, seq, rest) => {
            session_item::session_item_command(id, seq, rest).await
        }
        Command::Ready => ready_command().await,
        Command::Version
        | Command::SelfTest
        | Command::InstallBundle(..)
        | Command::Interactive(_) => {
            unreachable!("process-entry routes never construct the control future")
        }
        Command::Unknown(other) => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run (-p <prompt>|-|<prompt>) \
                 [--json|--output print|json|jsonl] [--timeout <dur>] \
                 [--max-tokens <n>] [--max-cost <usd>] [--max-time <dur>] [--seed <n>] \
                 [--request-tranche <n>] [--max-requests <n>] [--resume <run-id>] \
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
                 session provider rebind --session <id> --provider <id> [--base-url <url>] [--account <name>], \
                 session workspace set <path>, session <id> workspace set <path>, \
                 session retract --session <id> [--json], \
                 session <id> item <seq> --json [--masked] [--no-spawn], \
                 account list [--json], account use <alias> [--confirm], account source list [--json], account source add <codex|claude_file|grok|kimi_code_home> <root> [--label <label>], account source remove <source-id>, account source scan [--json], account import <codex|claude-code> [--confirm], account refresh <alias>, account remove <alias> --confirm, \
                 account add <alias> --base-url <url> [--api-key <key>|--api-key-env <VAR>|--api-key-stdin|--no-auth] [--api-family openai|anthropic] [--response-open-timeout <dur>] [--chunk-idle-timeout <dur>] [--semantic-progress-timeout <dur>] [--json], \
                 account probe <alias> [--json], account update <alias> [--base-url <url>] [--api-key <key>|--api-key-env <VAR>|--api-key-stdin] [--response-open-timeout <dur>] [--chunk-idle-timeout <dur>] [--semantic-progress-timeout <dur>] [--json], \
                 provider list [--json], provider show <name> [--json], provider add <name> --base-url <url> [--lockdown|--full] ..., provider set <name> (--lockdown|--full), provider remove <name> --confirm, \
                 lockdown status [--json], lockdown quota [--set <bytes>] [--json], \
                 models [--json] [--refresh [<alias>]], \
                 fleet [<session-id>] [--json] [--no-spawn], \
                 events [--follow] [--no-spawn], \
                 agent spawn|list|message|cancel|wait (agent --help), workflow run|status|list (workflow --help), \
                 graph status <session-id> [--json], graph pin <session-id>, \
                 graph abandon <session-id> [why], \
                 export <session-id> [--format markdown|json|codex|claude-code|opencode|pipe] [--out PATH] [--masked] [--confirm], \
                 hooks list [--json], hooks trust <digest>, hooks revoke <digest>, \
                 update [--check], \
                 tui [--theme system|light|dark|desert|oasis] [--session <id>] [--no-update-check], tui --demo [--plain], \
                 import [codex|claude-code], [--session <id>] [--no-update-check], --ready)"
            );
            ExitCode::from(2)
        }
    }
}
/// Additive options accepted by the bare `haider` live-TUI front door.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BareTuiOptions {
    pub session: Option<String>,
    pub no_update_check: bool,
    /// `haider resume` / `--resume` with no id: boot straight into the
    /// all-sessions browser so the user PICKS a session (owner 2026-08-21).
    pub browse_sessions: bool,
}

/// Parse only the bare-TUI argument vocabulary. `Ok(None)` leaves ordinary
/// subcommands to the main dispatcher.
pub fn parse_bare_tui_options(args: &[String]) -> Result<Option<BareTuiOptions>, String> {
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

async fn ready_command() -> ExitCode {
    let env = haider_client::ProfileEnv::capture();
    let profile = match haider_client::resolve_profile(&env) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let mut ensure_options = haider_client::EnsureOptions::default();
    {
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
            {
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
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("haider: {error}");
            ExitCode::from(front_door_exit_code(&error))
        }
    }
}
pub fn front_door_exit_code(error: &haider_client::EnsureError) -> u8 {
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

/// Explorer destroys a double-clicked console as soon as the process exits.
/// Preserve the already-printed failure until a key is pressed, but only when
/// Win32 proves this process is alone, every standard stream is still attached
/// to the console, and no CI marker makes the invocation non-interactive.
#[cfg(windows)]
pub fn hold_explorer_console_on_failure(code: ExitCode) {
    use std::io::{self, IsTerminal as _, Write as _};

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

#[cfg(test)]
mod runtime_tests {
    use super::footprint_hold_ms;
    #[test]
    fn footprint_hold_is_opt_in_and_bounded() {
        assert_eq!(footprint_hold_ms("60000"), Some(60_000));
        assert_eq!(footprint_hold_ms("0"), None);
        assert_eq!(footprint_hold_ms("300001"), None);
        assert_eq!(footprint_hold_ms("not-a-duration"), None);
    }
}
