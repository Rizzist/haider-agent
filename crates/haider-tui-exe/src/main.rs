//! On-demand interactive payload. The CLI library owns the command grammar.
use haider_tui::app::AppModel;
use haider_tui::demo_store::DemoStore;
use haider_tui::runtime::{LiveExit, detect_system_theme, run_demo, run_demo_plain, run_live};
use haider_tui::sanctum::SanctumTier;
use haider_tui::settings::SettingsStore;
use haider_tui::theme::ThemeChoice;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use haider_cli::routing::{Command, InteractiveCommand, parse_command};
use haider_cli::{BareTuiOptions, VERSION, front_door_exit_code};
use haider_protocol::ids::SessionId;
const EX_UNAVAILABLE: u8 = 69;
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;
mod update {
    pub use haider_cli::update::*;
    pub mod tui;
}

fn main() -> ExitCode {
    // The marker is read as data by offline self-test, without executing UI.
    const PAYLOAD_MARKER: &str = concat!("haider.payload.v1\0", env!("CARGO_PKG_VERSION"), "\0");
    std::hint::black_box(PAYLOAD_MARKER);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [v] if matches!(v.as_str(), "--version" | "-V" | "version")) {
        println!("haider-tui {VERSION}");
        return ExitCode::SUCCESS;
    }
    if let Some(expected) = std::env::var_os("HAIDER_TUI_LAUNCH_VERSION")
        && expected != VERSION
    {
        eprintln!("haider: interactive payload version mismatch; reinstall the matching bundle");
        return ExitCode::from(76);
    }
    #[cfg(not(windows))]
    {
        run_payload(args)
    }
    #[cfg(windows)]
    {
        let launched = std::thread::Builder::new()
            .name("haider-main".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || run_payload(args));
        let code = match launched {
            Ok(thread) => thread.join().unwrap_or_else(|_| {
                eprintln!("haider: main runtime thread panicked");
                ExitCode::from(EX_SOFTWARE)
            }),
            Err(error) => {
                eprintln!("haider: could not start main runtime thread: {error}");
                ExitCode::from(EX_SOFTWARE)
            }
        };
        haider_cli::hold_explorer_console_on_failure(code);
        code
    }
}

fn run_payload(args: Vec<String>) -> ExitCode {
    let action = match parse_command(&args) {
        Ok(Command::Interactive(action)) => action,
        Ok(_) => {
            eprintln!("haider-tui: use haider for control commands");
            return ExitCode::from(2);
        }
        Err(message) => {
            eprintln!("haider: {message}");
            return ExitCode::from(2);
        }
    };
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("haider: could not start async runtime: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    runtime.block_on(async {
        match action {
            InteractiveCommand::FrontDoor(options) => front_door_with_options(options).await,
            InteractiveCommand::Tui(rest) => tui_command(rest).await,
            InteractiveCommand::Ssh(rest) => {
                haider_cli::ssh::ssh_command_with_terminal(rest, &Terminal).await
            }
        }
    })
}

fn build_runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}

struct Terminal;
impl haider_cli::ssh::InteractiveTerminal for Terminal {
    fn run<'a>(
        &'a self,
        client: &'a haider_client::RpcClient,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<i32>, haider_cli::ssh::TerminalError>>
                + 'a,
        >,
    > {
        Box::pin(async move {
            haider_tui::ssh_terminal::run_ssh_terminal(client, name)
                .await
                .map_err(|error| match error {
                    haider_tui::ssh_terminal::SshTerminalError::Client(error) => {
                        haider_cli::ssh::TerminalError::Client(error)
                    }
                    haider_tui::ssh_terminal::SshTerminalError::Io(error) => {
                        haider_cli::ssh::TerminalError::Io(error)
                    }
                    other => haider_cli::ssh::TerminalError::Other(other.to_string()),
                })
        })
    }
}
/// ADE seam: optional `--session <id>` opens attached once the daemon's list
/// proves it exists; the update policy bit controls only automatic checks.
async fn front_door_with_options(options: BareTuiOptions) -> ExitCode {
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
    let interactive = std::io::IsTerminal::is_terminal(&io::stdout());
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
                        Ok(executable) => executable.with_file_name(if cfg!(windows) {
                            "haider.exe"
                        } else {
                            "haider"
                        }),
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
        return front_door_with_options(BareTuiOptions {
            session,
            no_update_check,
            browse_sessions: false,
        })
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

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn interactive_payload_keeps_the_two_worker_multithread_runtime() {
        let runtime = build_runtime().unwrap_or_else(|error| panic!("runtime: {error}"));
        assert_eq!(
            runtime.handle().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        );
        assert_eq!(runtime.metrics().num_workers(), 2);
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
