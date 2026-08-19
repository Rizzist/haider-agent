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

pub(crate) mod export;
pub(crate) mod graph;
pub(crate) mod hooks;
pub(crate) mod models;
pub(crate) mod observe;
pub(crate) mod run;
pub(crate) mod session_config;
pub(crate) mod update;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;

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

#[cfg(not(windows))]
#[tokio::main]
async fn main() -> ExitCode {
    dispatch().await
}

// The CLI dispatcher composes several large async command futures. Windows
// gives the executable's main thread a much smaller stack than Unix, so poll
// that dispatcher on an explicitly sized thread while retaining Tokio's
// normal multi-thread runtime for all spawned work.
#[cfg(windows)]
fn main() -> ExitCode {
    let launched = std::thread::Builder::new()
        .name("haider-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(dispatch()))
        });
    match launched {
        Ok(thread) => match thread.join() {
            Ok(Ok(code)) => code,
            Ok(Err(error)) => {
                eprintln!("haider: could not start async runtime: {error}");
                ExitCode::from(EX_SOFTWARE)
            }
            Err(_) => {
                eprintln!("haider: main runtime thread panicked");
                ExitCode::from(EX_SOFTWARE)
            }
        },
        Err(error) => {
            eprintln!("haider: could not start main runtime thread: {error}");
            ExitCode::from(EX_SOFTWARE)
        }
    }
}

async fn dispatch() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_bare_tui_options(&args) {
        Ok(Some(options)) => return front_door_with_options(FrontDoor::Tui, options).await,
        Ok(None) => {}
        Err(message) => {
            eprintln!("haider: {message}");
            return ExitCode::from(2);
        }
    }
    match args.as_slice() {
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
        [command, rest @ ..] if command == "sessions" => observe::sessions_command(rest).await,
        [command, session_id, subcommand, rest @ ..]
            if command == "session" && subcommand == "config" =>
        {
            session_config::session_config_command(session_id, rest).await
        }
        [command, rest @ ..] if command == "session" => observe::session_command(rest).await,
        [command, rest @ ..] if command == "models" => models::models_command(rest).await,
        [command, rest @ ..] if command == "fleet" => observe::fleet_command(rest).await,
        [command, rest @ ..] if command == "events" => observe::events_command(rest).await,
        [command, rest @ ..] if command == "graph" => graph::graph_command(rest).await,
        [command, rest @ ..] if command == "export" => export::export_command(rest).await,
        [command, rest @ ..] if command == "hooks" => hooks::hooks_command(rest).await,
        [command, rest @ ..] if command == "update" => update::update_command(rest).await,
        [command, rest @ ..] if command == "tui" => tui_command(rest).await,
        [command, rest @ ..] if command == "import" => import_command(rest).await,
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run <prompt> \
                 [--output print|json|jsonl] [--timeout <dur>] \
                 [--model <model|provider/model>] [--effort <level>] [--speed <fast|normal>] [--account <alias>] \
                 [--allow-writes] [--allow-exec] [--trust-hooks] [--attach <path>]..., \
                 status [--json] [--no-spawn], sessions [--json] [--no-spawn], \
                 session <id> [--json|--watch] [--no-spawn], \
                 session <id> config [--json] [--model <model|provider/model>] [--effort <level>] [--speed <fast|normal>] [--account <alias>], \
                 models [--json], \
                 fleet [<session-id>] [--json] [--no-spawn], \
                 events [--follow] [--no-spawn], \
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
        // Bare-TUI arguments were handled before command dispatch.
        [] => unreachable!("bare TUI dispatch is handled above"),
    }
}

/// Additive options accepted by the bare `haider` live-TUI front door.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BareTuiOptions {
    pub session: Option<String>,
    pub no_update_check: bool,
}

/// Parse only the bare-TUI argument vocabulary. `Ok(None)` leaves ordinary
/// subcommands to the main dispatcher.
pub(crate) fn parse_bare_tui_options(args: &[String]) -> Result<Option<BareTuiOptions>, String> {
    if args.is_empty() {
        return Ok(Some(BareTuiOptions::default()));
    }
    if !matches!(args[0].as_str(), "--session" | "--no-update-check") {
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
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| "--session requires a session id".to_owned())?;
                options.session = Some(id.clone());
            }
            "--session" => return Err("--session was supplied twice".into()),
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

    fn env_override(self) -> &'static str {
        match self {
            Self::Codex => "HAIDER_CODEX_AUTH_PATH",
            Self::ClaudeCode => "HAIDER_CLAUDE_CREDS_PATH",
        }
    }

    fn home_relative_path(self) -> &'static str {
        match self {
            Self::Codex => ".codex/auth.json",
            Self::ClaudeCode => ".claude/.credentials.json",
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
    let ImportDispatch::Source(source) = dispatch else {
        for source in [ImportSource::Codex, ImportSource::ClaudeCode] {
            let path = import_source_path(source);
            let status = if path.is_file() { "exists" } else { "missing" };
            println!("{}: {status} ({})", source.as_str(), path.display());
        }
        return ExitCode::SUCCESS;
    };
    let env = haider_client::ProfileEnv::capture();
    let profile = match haider_client::resolve_profile(&env) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut options = haider_client::EnsureOptions::default();
    options
        .required_features
        .insert(haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_V1.to_owned());
    let ensured = match haider_client::ensure_daemon(&profile, options).await {
        Ok(ensured) => ensured,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let response = ensured
        .client
        .request(haider_rpc::RequestBody::AccountOAuthImport {
            command_id: haider_rpc::CommandId::new(import_command_id()),
            source: source.as_str().to_owned(),
        })
        .await;
    ensured.client.close();
    match response {
        Ok(haider_rpc::ResponseBody::AccountOAuthImport { descriptor, .. }) => {
            println!(
                "imported {} ({}) — {}",
                descriptor.alias, descriptor.provider, descriptor.identity
            );
            ExitCode::SUCCESS
        }
        Ok(haider_rpc::ResponseBody::Error { message, .. }) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        Ok(_) => {
            eprintln!("daemon returned an unexpected response to account.oauth_import");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn import_source_path(source: ImportSource) -> PathBuf {
    if let Some(path) = std::env::var_os(source.env_override()).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("~").join(source.home_relative_path()),
        |home| PathBuf::from(home).join(source.home_relative_path()),
    )
}

fn import_command_id() -> String {
    format!(
        "oauth-import-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    )
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
    } = options;
    let env = haider_client::ProfileEnv::capture();
    let profile = match haider_client::resolve_profile(&env) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    match haider_client::ensure_daemon(&profile, haider_client::EnsureOptions::default()).await {
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
            let interactive =
                mode == FrontDoor::Tui && std::io::IsTerminal::is_terminal(&io::stdout());
            if !interactive {
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
                ensured.client.close();
                return ExitCode::SUCCESS;
            }
            let mut model = live_model(&profile);
            model.initial_session = initial_session.map(haider_protocol::ids::SessionId::new);
            let updates =
                update::tui::live_update_bridge(profile.store_dir.clone(), no_update_check);
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
            "--session" => match iter.next().filter(|id| !id.is_empty()) {
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
