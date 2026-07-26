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
use haider_tui::runtime::{run_demo, run_demo_plain};
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

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [version] if matches!(version.as_str(), "--version" | "-V" | "version") => {
            println!("haider {VERSION}");
            ExitCode::SUCCESS
        }
        [command] if command == "self-test" => self_test().await,
        [command, rest @ ..] if command == "run" => run_command(rest).await,
        [command, rest @ ..] if command == "tui" => tui_command(rest).await,
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run --jsonl <prompt>, \
                 run --jsonl --provider anthropic --model <id> <prompt>, \
                 tui --demo [--plain] [--theme dawn|ivory|dark])"
            );
            ExitCode::from(2)
        }
        [] => {
            println!(
                "haider {VERSION} — run `haider self-test`, \
                 `haider run --jsonl \"<prompt>\"`, or `haider tui --demo`"
            );
            ExitCode::SUCCESS
        }
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
    let mut theme = ThemeKey::Dawn;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--plain" => plain = true,
            "--theme" => match iter.next().and_then(|name| ThemeKey::parse(name)) {
                Some(key) => theme = key,
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
        eprintln!("haider tui: only `haider tui --demo` is available until the daemon lands");
        return ExitCode::from(2);
    }
    let mut model = AppModel::new();
    model.theme = theme;
    // Translit is the terminal default (no bidi/shaping in ratatui);
    // HAIDER_SHAHADA=arabic opts shaping-capable terminals into Arabic.
    if matches!(std::env::var("HAIDER_SHAHADA").as_deref(), Ok("arabic")) {
        model.sanctum_tier = SanctumTier::Arabic;
    }
    if plain || !io::stdout().is_terminal() {
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
    match run_demo(model).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("haider tui: terminal error: {error}");
            ExitCode::from(EX_IOERR)
        }
    }
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
