//! `haider` — the Haider Code harness binary.

use haider_core::{
    HarnessActor, HarnessConfig, MemoryStore, SqliteStoreHandle, StoreHandle, SubmitTurn,
    TurnOutcome,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::{FinishReason, Usage, UsageSource};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider};
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
        [command, framing, prompt] if command == "run" && framing == "--jsonl" => {
            run_jsonl(prompt).await
        }
        [other, ..] => {
            eprintln!(
                "haider: unknown or incomplete command `{other}` \
                 (supports: --version, self-test, run --jsonl <prompt>)"
            );
            ExitCode::from(2)
        }
        [] => {
            println!(
                "haider {VERSION} — run `haider self-test` or \
                 `haider run --jsonl \"<prompt>\"`"
            );
            ExitCode::SUCCESS
        }
    }
}

/// Runs one fake-provider turn, streaming every committed envelope to stdout
/// as LF-framed JSONL. Exit codes: Done → 0, provider-errored turn or bad
/// fixture → 65, harness/stdout faults → 70/74, cancellation → 130.
async fn run_jsonl(prompt: &str) -> ExitCode {
    let provider = match provider_for_cli(prompt) {
        Ok(provider) => Arc::new(provider),
        Err(error) => {
            eprintln!("haider: invalid fake-provider fixture: {error}");
            return ExitCode::from(EX_DATAERR);
        }
    };
    let profile_dir = match cli_profile_dir() {
        Ok(profile_dir) => profile_dir,
        Err(error) => {
            eprintln!("haider: {}", error.message);
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let store = match SqliteStoreHandle::open(profile_dir).await {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("haider: cannot open profile: {}", error.message);
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let outcome = match stream_jsonl_turn(prompt, cli_config(), provider, store, &mut output).await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let exit_code = error.exit_code();
            eprintln!("haider: {error}");
            return ExitCode::from(exit_code);
        }
    };

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

/// Fixture comes from `HAIDER_FAKE_SCRIPT_JSON` when set (the test hook);
/// otherwise a canned happy-path turn that echoes the prompt.
fn provider_for_cli(prompt: &str) -> Result<FakeProvider, serde_json::Error> {
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

fn cli_config() -> HarnessConfig {
    HarnessConfig::for_session(
        SessionId::new("cli-session"),
        DeviceId::new("cli-device"),
        1,
        1,
    )
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
    let handle = HarnessActor::spawn(cli_config(), provider, store.clone());
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
