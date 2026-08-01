//! `haider` — the Haider Code harness binary.

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use haider_tui::app::AppModel;
use haider_tui::demo_store::DemoStore;
use haider_tui::runtime::{detect_system_theme, run_demo, run_demo_plain, run_live};
use haider_tui::sanctum::SanctumTier;
use haider_tui::theme::ThemeKey;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

pub(crate) mod run;
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

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        [command, rest @ ..] if command == "update" => update::update_command(rest).await,
        [command, rest @ ..] if command == "tui" => tui_command(rest).await,
        [command, rest @ ..] if command == "import" => import_command(rest).await,
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run <prompt> \
                 [--output print|json|jsonl] [--timeout <dur>] \
                 [--allow-writes] [--allow-exec] [--attach <path>]..., \
                 update [--check], \
                 tui [--theme dawn|ivory|dark], tui --demo [--plain], \
                 import [codex|claude-code], --ready)"
            );
            ExitCode::from(2)
        }
        // The keystone front door (report R8 + R11): bare `haider` connects
        // to — or spawns — the profile daemon and enters the LIVE TUI on
        // that connection.
        [] => front_door(FrontDoor::Tui).await,
    }
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
            let model = live_model(&profile);
            match run_live(model, ensured.client, profile, live_client_config()).await {
                Ok(()) => ExitCode::SUCCESS,
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

/// `haider tui --demo [--plain] [--theme dawn|ivory|dark]` — the scripted
/// demo drives every surface until the daemon lands (W3). `--plain` (or a
/// non-TTY stdout, research rec 2) renders the final state as plain text
/// instead of taking the terminal. `HAIDER_SHAHADA=translit` selects the
/// transliteration sanctum tier.
async fn tui_command(rest: &[String]) -> ExitCode {
    use std::io::IsTerminal;
    let mut demo = false;
    let mut plain = false;
    let mut theme: Option<ThemeKey> = None;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--plain" => plain = true,
            "--theme" => match iter.next().and_then(|name| ThemeKey::parse(name)) {
                Some(key) => theme = Some(key),
                None => {
                    eprintln!("haider tui: --theme takes dawn|ivory|dark");
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
        return front_door(FrontDoor::Tui).await;
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
        // save — persistence is an interactive-session affordance.
        model.theme = theme.unwrap_or(ThemeKey::Dawn);
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
    // theme decision so a persisted theme can win over system detection.
    // A missing/corrupt file or an unresolvable path simply keeps the
    // seeds; the demo never fails to start over persistence.
    let mut theme_restored = false;
    let store = DemoStore::default_path().map(|path| {
        let store = DemoStore::at(path);
        if let Some(dto) = store.load() {
            theme_restored = haider_tui::demo_store::hydrate(&mut model, dto).theme_restored;
        }
        store
    });
    // Explicit --theme wins; then the persisted theme (sim: a known
    // `data.themeName` restores); otherwise follow the system/terminal
    // appearance (OSC 11 background luminance): dark ground -> Dark,
    // light -> Dawn.
    if let Some(key) = theme {
        model.theme = key;
    } else if !theme_restored {
        model.theme = detect_system_theme();
    }
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
    model.theme = detect_system_theme();
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
