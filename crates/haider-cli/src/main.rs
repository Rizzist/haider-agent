//! `haider` — the Haider Code harness binary.

use haider_accounts::{
    AccountStore, AuthMethod, CredentialAlias, CredentialDescriptor, CredentialStatus,
    JsonFileStore, KeychainVault, Resolver, RotationCallback, RotationDecision, import_env,
};
use haider_core::{
    HarnessActor, HarnessConfig, MemoryStore, SqliteStoreHandle, StoreHandle, SubmitTurn,
    TurnOutcome,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::{FinishReason, Usage, UsageSource};
use haider_protocol::state::RunState;
use haider_provider::{
    ANTHROPIC_PROVIDER_NAME, AnthropicProvider, FakeProvider, FakeStep, Provider, ProviderError,
};
use haider_tui::app::AppModel;
use haider_tui::demo_store::DemoStore;
use haider_tui::runtime::{detect_system_theme, run_demo, run_demo_plain, run_live};
use haider_tui::sanctum::SanctumTier;
use haider_tui::theme::ThemeKey;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const EX_DATAERR: u8 = 65;
const EX_SOFTWARE: u8 = 70;
const EX_IOERR: u8 = 74;
const EX_CANCELLED: u8 = 130;
const FAKE_SCRIPT_ENV: &str = "HAIDER_FAKE_SCRIPT_JSON";
const PROFILE_DIR_ENV: &str = "HAIDER_PROFILE_DIR";
const ANTHROPIC_KEY_ENV: &str = "HAIDER_ANTHROPIC_API_KEY";
const READ_PAGE_SIZE: usize = 256;

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
        [command, rest @ ..] if command == "run" => run_command(rest).await,
        [command, rest @ ..] if command == "tui" => tui_command(rest).await,
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run --jsonl <prompt>, \
                 run --jsonl --provider anthropic --model <id> <prompt>, \
                 tui [--theme dawn|ivory|dark], tui --demo [--plain], --ready)"
            );
            ExitCode::from(2)
        }
        // The keystone front door (report R8 + R11): bare `haider` connects
        // to — or spawns — the profile daemon and enters the LIVE TUI on
        // that connection.
        [] => front_door(FrontDoor::Tui).await,
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

async fn run_command(rest: &[String]) -> ExitCode {
    let options = match parse_run_options(rest) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("haider run: {message}");
            return ExitCode::from(2);
        }
    };
    run_jsonl(options).await
}

/// Runs one provider turn, streaming every committed envelope to stdout as
/// LF-framed JSONL. Exit codes: Done → 0, provider/setup errors → 65,
/// harness/stdout faults → 70/74, cancellation → 130.
async fn run_jsonl(options: RunOptions) -> ExitCode {
    let profile_dir = match cli_profile_dir() {
        Ok(profile_dir) => profile_dir,
        Err(error) => {
            eprintln!("haider: {}", error.message);
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let provider = match provider_for_cli(&options, &profile_dir) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_DATAERR);
        }
    };
    let store = match SqliteStoreHandle::open(profile_dir).await {
        Ok(store) => store,
        Err(error) => {
            eprintln!("haider: cannot open profile: {}", error.message);
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let config = cli_config(store.worker_generation(), &options.model);
    let actor_store: Arc<dyn StoreHandle> = Arc::new(store.clone());
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let run_result =
        stream_jsonl_turn(&options.prompt, config, provider, actor_store, &mut output).await;
    let close_result = store.close().await;
    let outcome = match run_result {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Err(close_error) = close_result {
                eprintln!("haider: profile close also failed: {}", close_error.message);
            }
            let exit_code = error.exit_code();
            eprintln!("haider: {error}");
            return ExitCode::from(exit_code);
        }
    };
    if let Err(error) = close_result {
        eprintln!("haider: cannot close profile: {}", error.message);
        return ExitCode::from(EX_SOFTWARE);
    }

    if outcome.state == RunState::Errored {
        if let Some(error) = &outcome.error {
            eprintln!("haider: {}", error.message);
        }
    } else if !matches!(outcome.state, RunState::Done | RunState::Cancelled) {
        eprintln!("haider: turn ended in unexpected state {:?}", outcome.state);
    }
    ExitCode::from(exit_code_for_outcome(&outcome))
}

/// Internal injectable runner shared with the CLI integration tests. The live
/// broadcast is only a wake-up hint: every wake drains committed envelopes by
/// sequence, so lag cannot create gaps.
pub(crate) async fn stream_jsonl_turn<W: Write + ?Sized>(
    prompt: &str,
    config: HarnessConfig,
    provider: Arc<dyn Provider>,
    store: Arc<dyn StoreHandle>,
    output: &mut W,
) -> Result<TurnOutcome, JsonlRunError> {
    let session_id = config.session_id.clone();
    let mut last_seq = store
        .latest_seq(&session_id)
        .await
        .map_err(JsonlRunError::Runtime)?;
    let handle = HarnessActor::spawn(config, provider, Arc::clone(&store));
    let mut events = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new(prompt))
        .await
        .map_err(JsonlRunError::Runtime)?;
    let mut outcome = Box::pin(turn.wait());
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut outcome => {
                break result.map_err(JsonlRunError::Runtime)?;
            }
            event = events.recv() => {
                match event {
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        drain_committed(
                            store.as_ref(),
                            &session_id,
                            &mut last_seq,
                            output,
                        )
                        .await?;
                    }
                    Err(RecvError::Closed) => {
                        return Err(JsonlRunError::EventStreamClosed);
                    }
                }
            }
        }
    };

    // Outcome publication happens after the actor's final append attempt, so
    // this drain captures every envelope that can belong to the turn.
    drain_committed(store.as_ref(), &session_id, &mut last_seq, output).await?;
    Ok(outcome)
}

pub(crate) fn exit_code_for_outcome(outcome: &TurnOutcome) -> u8 {
    match outcome.state {
        RunState::Done => 0,
        RunState::Cancelled => EX_CANCELLED,
        RunState::Errored
            if outcome.error.as_ref().is_some_and(|error| {
                matches!(
                    error.code,
                    ErrorCode::ProviderError | ErrorCode::ProviderTimeout
                )
            }) =>
        {
            EX_DATAERR
        }
        RunState::Errored
        | RunState::Queued
        | RunState::Thinking
        | RunState::Streaming
        | RunState::RunningTool
        | RunState::Waiting { .. }
        | RunState::InputRequired { .. }
        | RunState::PermissionRequired { .. }
        | RunState::Compacting
        | RunState::Verifying { .. }
        | RunState::Concluding
        | RunState::EffectOutcomeUnknown
        | RunState::Cancelling => EX_SOFTWARE,
    }
}

#[derive(Debug)]
pub(crate) enum JsonlRunError {
    Runtime(HaiderError),
    Output(io::Error),
    EventStreamClosed,
}

impl JsonlRunError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Output(_) => EX_IOERR,
            Self::Runtime(_) | Self::EventStreamClosed => EX_SOFTWARE,
        }
    }
}

impl fmt::Display for JsonlRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => formatter.write_str(&error.message),
            Self::Output(error) => write!(formatter, "stdout failed: {error}"),
            Self::EventStreamClosed => {
                formatter.write_str("event stream closed before the turn outcome")
            }
        }
    }
}

impl std::error::Error for JsonlRunError {}

async fn drain_committed<W: Write + ?Sized>(
    store: &dyn StoreHandle,
    session_id: &SessionId,
    last_seq: &mut u64,
    output: &mut W,
) -> Result<(), JsonlRunError> {
    loop {
        let envelopes = store
            .read(session_id, *last_seq, READ_PAGE_SIZE)
            .await
            .map_err(JsonlRunError::Runtime)?;
        if envelopes.is_empty() {
            return Ok(());
        }
        let page_len = envelopes.len();
        for envelope in envelopes {
            let expected = last_seq.checked_add(1).ok_or_else(|| {
                JsonlRunError::Runtime(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "session sequence space exhausted while streaming JSONL",
                    false,
                ))
            })?;
            if envelope.seq != expected {
                return Err(JsonlRunError::Runtime(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "committed JSONL replay expected sequence {expected}, got {}",
                        envelope.seq
                    ),
                    false,
                )));
            }
            write_jsonl(output, &envelope).map_err(JsonlRunError::Output)?;
            *last_seq = envelope.seq;
        }
        if page_len < READ_PAGE_SIZE {
            return Ok(());
        }
    }
}

fn write_jsonl(output: &mut (impl Write + ?Sized), envelope: &RawEnvelope) -> io::Result<()> {
    let line = serde_json::to_vec(envelope).map_err(io::Error::other)?;
    output.write_all(&line)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSelection {
    Fake,
    Anthropic,
}

#[derive(Debug)]
struct RunOptions {
    prompt: String,
    provider: ProviderSelection,
    model: String,
}

fn parse_run_options(rest: &[String]) -> Result<RunOptions, String> {
    let mut jsonl = false;
    let mut provider = ProviderSelection::Fake;
    let mut model = None;
    let mut prompt = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--jsonl" if !jsonl => jsonl = true,
            "--jsonl" => return Err("duplicate --jsonl flag".into()),
            "--provider" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--provider requires fake|anthropic".to_owned())?;
                provider = match value.as_str() {
                    "fake" => ProviderSelection::Fake,
                    ANTHROPIC_PROVIDER_NAME => ProviderSelection::Anthropic,
                    _ => return Err(format!("unknown provider `{value}`; use fake|anthropic")),
                };
            }
            "--model" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--model requires a model id".to_owned())?;
                if value.is_empty() {
                    return Err("--model requires a non-empty model id".into());
                }
                model = Some(value.clone());
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            value if prompt.is_none() => prompt = Some(value.to_owned()),
            _ => return Err("exactly one prompt argument is required".into()),
        }
        index += 1;
    }
    if !jsonl {
        return Err("--jsonl is required".into());
    }
    let prompt = prompt.ok_or_else(|| "a prompt argument is required".to_owned())?;
    let model = match (provider, model) {
        (ProviderSelection::Fake, model) => model.unwrap_or_else(|| "fake-model".into()),
        (ProviderSelection::Anthropic, Some(model)) => model,
        (ProviderSelection::Anthropic, None) => {
            return Err("--provider anthropic requires --model <id>".into());
        }
    };
    Ok(RunOptions {
        prompt,
        provider,
        model,
    })
}

#[derive(Debug)]
enum CliProviderError {
    FakeFixture(serde_json::Error),
    Accounts(HaiderError),
    Adapter(ProviderError),
}

impl fmt::Display for CliProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FakeFixture(error) => write!(formatter, "invalid fake-provider fixture: {error}"),
            Self::Accounts(error) => formatter.write_str(&error.message),
            Self::Adapter(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CliProviderError {}

fn provider_for_cli(
    options: &RunOptions,
    profile_dir: &std::path::Path,
) -> Result<Arc<dyn Provider>, CliProviderError> {
    match options.provider {
        ProviderSelection::Fake => fake_provider_for_cli(&options.prompt)
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            .map_err(CliProviderError::FakeFixture),
        ProviderSelection::Anthropic => anthropic_provider_for_cli(profile_dir, &options.model)
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>),
    }
}

/// Fixture comes from `HAIDER_FAKE_SCRIPT_JSON` when set (the test hook);
/// otherwise a canned happy-path turn that echoes the prompt.
fn fake_provider_for_cli(prompt: &str) -> Result<FakeProvider, serde_json::Error> {
    match std::env::var(FAKE_SCRIPT_ENV) {
        Ok(script) => FakeProvider::from_json(&script),
        Err(_) => Ok(FakeProvider::new(vec![
            FakeStep::EmitText {
                text: format!("fake response: {prompt}"),
            },
            FakeStep::EmitUsage {
                usage: Usage {
                    input: 0,
                    output: 0,
                    reasoning: 0,
                    cached: 0,
                    source: UsageSource::LocallyExact,
                    account: None,
                },
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])),
    }
}

fn anthropic_provider_for_cli(
    profile_dir: &std::path::Path,
    model: &str,
) -> Result<AnthropicProvider, CliProviderError> {
    let mut accounts =
        AccountStore::new(JsonFileStore::new(profile_dir)).map_err(CliProviderError::Accounts)?;
    let vault = KeychainVault::new();
    if accounts
        .active_for_provider(ANTHROPIC_PROVIDER_NAME)
        .is_none()
    {
        let alias = import_env(&vault, ANTHROPIC_PROVIDER_NAME, ANTHROPIC_KEY_ENV)
            .map_err(CliProviderError::Accounts)?;
        accounts
            .add(CredentialDescriptor {
                alias,
                provider: ANTHROPIC_PROVIDER_NAME.into(),
                auth_method: AuthMethod::ApiKey,
                identity: format!("{ANTHROPIC_KEY_ENV} import"),
                status: CredentialStatus::Ok,
                active: true,
            })
            .map_err(CliProviderError::Accounts)?;
    }
    let rotation = StopRotation;
    let resolver = Resolver::new(&accounts, &vault, &rotation);
    let (descriptor, credential) = resolver
        .resolve_for_provider(ANTHROPIC_PROVIDER_NAME)
        .map_err(CliProviderError::Accounts)?;
    AnthropicProvider::new(credential, model)
        .map(|provider| provider.with_account(descriptor.alias))
        .map_err(CliProviderError::Adapter)
}

struct StopRotation;

impl RotationCallback for StopRotation {
    fn on_limited(&self, _alias: &CredentialAlias, _until_ms: u64) -> RotationDecision {
        RotationDecision::Stop
    }
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

fn cli_profile_dir() -> Result<PathBuf, HaiderError> {
    if let Some(profile_dir) = std::env::var_os(PROFILE_DIR_ENV) {
        return Ok(PathBuf::from(profile_dir));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("{PROFILE_DIR_ENV} is unset and HOME is unavailable"),
            false,
        )
    })?;
    Ok(PathBuf::from(home).join(".haider").join("dev-profile"))
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
