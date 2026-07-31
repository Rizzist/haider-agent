//! Manual `haider run` parser and daemon-backed output adapter.

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use haider_client::{
    ConnectError, EnsureError, EnsureOptions, HeadlessEvent, HeadlessFailureCode, HeadlessOutcome,
    HeadlessRunError, HeadlessRunRequest, HeadlessRunResult, ProfileEnv, resolve_profile,
    run_headless,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::session::SessionPermissionOverridesV1;
use tokio::sync::mpsc;

pub(crate) const EX_USAGE: u8 = 2;
pub(crate) const EX_PROVIDER: u8 = 65;
pub(crate) const EX_UNAVAILABLE: u8 = 69;
pub(crate) const EX_SOFTWARE: u8 = 70;
pub(crate) const EX_IOERR: u8 = 74;
pub(crate) const EX_PROTOCOL: u8 = 76;
pub(crate) const EX_BLOCKED: u8 = 77;
pub(crate) const EX_TIMEOUT: u8 = 124;
pub(crate) const EX_CANCELLED: u8 = 130;

const OUTPUT_BUFFER: usize = 64;
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOutput {
    Print,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderSelection {
    Fake,
    Anthropic,
}

impl ProviderSelection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fake => "fake",
            Self::Anthropic => haider_provider::ANTHROPIC_PROVIDER_NAME,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub prompt: String,
    pub output: RunOutput,
    pub timeout: Option<Duration>,
    pub allow_writes: bool,
    pub allow_exec: bool,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
}

pub(crate) fn parse_run_options(rest: &[String]) -> Result<RunOptions, String> {
    let mut output = None;
    let mut legacy_jsonl = false;
    let mut timeout = None;
    let mut allow_writes = false;
    let mut allow_exec = false;
    let mut provider = None;
    let mut model = None;
    let mut prompt = None;
    let mut index = 0;

    while index < rest.len() {
        match rest[index].as_str() {
            "--jsonl" if !legacy_jsonl => legacy_jsonl = true,
            "--jsonl" => return Err("duplicate --jsonl flag".into()),
            "--output" if output.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--output requires print|json|jsonl".to_owned())?;
                output = Some(match value.as_str() {
                    "print" => RunOutput::Print,
                    "json" => RunOutput::Json,
                    "jsonl" => RunOutput::Jsonl,
                    _ => return Err(format!("unknown output `{value}`; use print|json|jsonl")),
                });
            }
            "--output" => return Err("duplicate --output flag".into()),
            "--timeout" if timeout.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--timeout requires a duration".to_owned())?;
                timeout = Some(parse_timeout(value)?);
            }
            "--timeout" => return Err("duplicate --timeout flag".into()),
            "--allow-writes" if !allow_writes => allow_writes = true,
            "--allow-writes" => return Err("duplicate --allow-writes flag".into()),
            "--allow-exec" if !allow_exec => allow_exec = true,
            "--allow-exec" => return Err("duplicate --allow-exec flag".into()),
            "--provider" if provider.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--provider requires fake|anthropic".to_owned())?;
                provider = Some(match value.as_str() {
                    "fake" => ProviderSelection::Fake,
                    haider_provider::ANTHROPIC_PROVIDER_NAME => ProviderSelection::Anthropic,
                    _ => return Err(format!("unknown provider `{value}`; use fake|anthropic")),
                });
            }
            "--provider" => return Err("duplicate --provider flag".into()),
            "--model" if model.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--model requires a model id".to_owned())?;
                if value.is_empty() {
                    return Err("--model requires a non-empty model id".into());
                }
                model = Some(value.clone());
            }
            "--model" => return Err("duplicate --model flag".into()),
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            value if prompt.is_none() => prompt = Some(value.to_owned()),
            _ => return Err("exactly one prompt argument is required".into()),
        }
        index += 1;
    }

    let output = match (legacy_jsonl, output) {
        (true, Some(RunOutput::Jsonl) | None) => RunOutput::Jsonl,
        (true, Some(_)) => return Err("--jsonl conflicts with a non-jsonl --output".into()),
        (false, Some(output)) => output,
        (false, None) => RunOutput::Print,
    };
    let prompt = prompt.ok_or_else(|| "a prompt argument is required".to_owned())?;
    if provider == Some(ProviderSelection::Anthropic) && model.is_none() {
        return Err("--provider anthropic requires --model <id>".into());
    }

    Ok(RunOptions {
        prompt,
        output,
        timeout,
        allow_writes,
        allow_exec,
        provider,
        model,
    })
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_u64)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_000)
    } else if let Some(digits) = value.strip_suffix('h') {
        (digits, 3_600_000)
    } else {
        return Err("--timeout requires an integer followed by ms, s, m, or h".into());
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| "--timeout requires a positive integer duration".to_owned())?;
    let millis = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "--timeout is too large".to_owned())?;
    let duration = Duration::from_millis(millis);
    if duration.is_zero() {
        return Err("--timeout must be greater than zero".into());
    }
    if duration > MAX_TIMEOUT {
        return Err("--timeout must not exceed 24h".into());
    }
    Ok(duration)
}

pub(crate) async fn run_command(rest: &[String]) -> ExitCode {
    let options = match parse_run_options(rest) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("haider run: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let provider = options.provider.map_or_else(
        || profile.default_provider.clone(),
        |value| value.as_str().into(),
    );
    let model = options.model.clone().unwrap_or_else(|| {
        if options.provider == Some(ProviderSelection::Fake) {
            "fake-model".into()
        } else {
            profile.default_model.clone()
        }
    });
    let cwd = match std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
    {
        Some(cwd) => cwd,
        None => {
            eprintln!("haider: current directory is unavailable or is not valid UTF-8");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let request = HeadlessRunRequest {
        cwd,
        prompt: options.prompt.clone(),
        provider,
        model,
        max_tokens: profile.default_max_tokens,
        permission_overrides: SessionPermissionOverridesV1 {
            allow_writes: options.allow_writes,
            allow_exec: options.allow_exec,
        },
        timeout: options.timeout,
        terminal_grace: haider_client::DEFAULT_TERMINAL_GRACE,
    };

    let (events, receiver) = mpsc::channel(OUTPUT_BUFFER);
    let output_mode = options.output;
    let adapter = tokio::task::spawn_blocking(move || adapt_events(output_mode, receiver));
    let result = run_headless(&profile, EnsureOptions::default(), request, events).await;
    let adapter_result = match adapter.await {
        Ok(result) => result,
        Err(error) => Err(io::Error::other(format!("output adapter failed: {error}"))),
    };
    if let Err(error) = adapter_result {
        eprintln!("haider: stdout failed: {error}");
        return ExitCode::from(EX_IOERR);
    }

    match result {
        Ok(result) => {
            if options.output != RunOutput::Jsonl
                && let Err(error) = write_final(io::stdout().lock(), options.output, &result)
            {
                eprintln!("haider: stdout failed: {error}");
                return ExitCode::from(EX_IOERR);
            }
            if result.outcome != HeadlessOutcome::Done
                && let Some(failure) = &result.failure
            {
                eprintln!("haider: {}", failure.message);
            }
            ExitCode::from(exit_code_for_result(&result))
        }
        Err(error) => {
            eprintln!("haider: {error}");
            ExitCode::from(exit_code_for_error(&error))
        }
    }
}

fn adapt_events(output: RunOutput, mut events: mpsc::Receiver<HeadlessEvent>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = io::BufWriter::new(stdout.lock());
    while let Some(event) = events.blocking_recv() {
        match event {
            HeadlessEvent::Envelope(envelope) if output == RunOutput::Jsonl => {
                serde_json::to_writer(&mut stdout, envelope.as_ref()).map_err(io::Error::other)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            HeadlessEvent::PermissionDenied(denial) => {
                eprintln!("haider: denied permission: {}", denial.effect_summary);
            }
            HeadlessEvent::Envelope(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn write_final(
    mut output: impl Write,
    mode: RunOutput,
    result: &HeadlessRunResult,
) -> io::Result<()> {
    match mode {
        RunOutput::Print => {
            if let Some(response) = &result.response {
                output.write_all(response.as_bytes())?;
                output.write_all(b"\n")?;
            }
        }
        RunOutput::Json => {
            output.write_all(run_json(result)?.as_bytes())?;
            output.write_all(b"\n")?;
        }
        RunOutput::Jsonl => {}
    }
    output.flush()
}

fn run_json(result: &HeadlessRunResult) -> io::Result<String> {
    let session_id = serde_json::to_string(result.session_id.as_str()).map_err(io::Error::other)?;
    let run_id = serde_json::to_string(result.run_id.as_str()).map_err(io::Error::other)?;
    let outcome = serde_json::to_string(&result.outcome).map_err(io::Error::other)?;
    let response = serde_json::to_string(&result.response).map_err(io::Error::other)?;
    let usage = serde_json::to_string(&result.usage).map_err(io::Error::other)?;
    let denials = serde_json::to_string(&result.permission_denials).map_err(io::Error::other)?;
    let error = match &result.failure {
        Some(failure) => format!(
            "{{\"code\":{},\"message\":{},\"retryable\":{}}}",
            serde_json::to_string(failure.code.as_str()).map_err(io::Error::other)?,
            serde_json::to_string(&failure.message).map_err(io::Error::other)?,
            failure.retryable,
        ),
        None => "null".into(),
    };
    Ok(format!(
        "{{\"schema\":\"haider.run.v1\",\"session_id\":{session_id},\"run_id\":{run_id},\"outcome\":{outcome},\"response\":{response},\"usage\":{usage},\"permission_denials\":{denials},\"error\":{error}}}"
    ))
}

pub(crate) fn exit_code_for_result(result: &HeadlessRunResult) -> u8 {
    match result.outcome {
        HeadlessOutcome::Done => 0,
        HeadlessOutcome::Cancelled => EX_CANCELLED,
        HeadlessOutcome::Timeout => EX_TIMEOUT,
        HeadlessOutcome::InputRequired => EX_BLOCKED,
        HeadlessOutcome::Errored => match result.failure.as_ref().map(|failure| &failure.code) {
            Some(HeadlessFailureCode::Run(
                ErrorCode::ProviderError
                | ErrorCode::ProviderTimeout
                | ErrorCode::CredentialMissing
                | ErrorCode::CredentialLimited,
            )) => EX_PROVIDER,
            Some(HeadlessFailureCode::Run(
                ErrorCode::PermissionDenied | ErrorCode::EffectUnknownOutcome,
            ))
            | Some(HeadlessFailureCode::Blocked(_)) => EX_BLOCKED,
            Some(HeadlessFailureCode::Run(ErrorCode::ProtocolMismatch)) => EX_PROTOCOL,
            _ => EX_SOFTWARE,
        },
    }
}

pub(crate) fn exit_code_for_error(error: &HeadlessRunError) -> u8 {
    match error {
        HeadlessRunError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | HeadlessRunError::Protocol { .. } => EX_PROTOCOL,
        HeadlessRunError::Ensure(EnsureError::Connect(
            ConnectError::Rejected(_) | ConnectError::Frame(_) | ConnectError::UnexpectedFrame,
        )) => EX_PROTOCOL,
        HeadlessRunError::Ensure(
            EnsureError::Connect(_)
            | EnsureError::Spawn { .. }
            | EnsureError::DaemonExited { .. }
            | EnsureError::StartupTimeout { .. },
        ) => EX_UNAVAILABLE,
        HeadlessRunError::Transport { .. } => EX_IOERR,
        HeadlessRunError::Encode { .. } => EX_SOFTWARE,
        HeadlessRunError::Rpc { code, .. } if code == "timeout_before_acceptance" => EX_TIMEOUT,
        HeadlessRunError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "provider_error"
                    | "provider_timeout"
                    | "credential_missing"
                    | "credential_limited"
                    | "unauthorized"
            ) =>
        {
            EX_PROVIDER
        }
        HeadlessRunError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "protocol_mismatch" | "unknown_method" | "invalid_argument"
            ) =>
        {
            EX_PROTOCOL
        }
        HeadlessRunError::Rpc { .. } => EX_SOFTWARE,
    }
}
