//! Manual `haider run` parser and daemon-backed output adapter.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use haider_client::{
    ConnectError, ERROR_CODE_NO_ACTIVE_ACCOUNT, ERROR_CODE_NO_DEFAULT_MODEL, EnsureError,
    EnsureOptions, HeadlessAttachment, HeadlessEvent, HeadlessFailureCode, HeadlessOutcome,
    HeadlessRunError, HeadlessRunRequest, HeadlessRunResult, ProfileEnv, load_image_attachment,
    load_text_attachment, resolve_profile, run_headless,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSelection(String);

impl ProviderSelection {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn is_fake(&self) -> bool {
        self.0 == "fake"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub prompt: String,
    pub output: RunOutput,
    pub timeout: Option<Duration>,
    pub allow_writes: bool,
    pub allow_exec: bool,
    pub trust_hooks: bool,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub attachments: Vec<PathBuf>,
}

pub(crate) fn parse_run_options(rest: &[String]) -> Result<RunOptions, String> {
    let mut output = None;
    let mut legacy_jsonl = false;
    let mut timeout = None;
    let mut allow_writes = false;
    let mut allow_exec = false;
    let mut trust_hooks = false;
    let mut provider = None;
    let mut model = None;
    let mut attachments = Vec::new();
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
            "--trust-hooks" if !trust_hooks => trust_hooks = true,
            "--trust-hooks" => return Err("duplicate --trust-hooks flag".into()),
            "--provider" if provider.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--provider requires a provider name".to_owned())?;
                if value.is_empty() {
                    return Err("--provider requires a non-empty provider name".into());
                }
                provider = Some(ProviderSelection(value.clone()));
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
            "--attach" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--attach requires a file path".to_owned())?;
                if value.is_empty() {
                    return Err("--attach requires a non-empty file path".into());
                }
                attachments.push(PathBuf::from(value));
            }
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
    Ok(RunOptions {
        prompt,
        output,
        timeout,
        allow_writes,
        allow_exec,
        trust_hooks,
        provider,
        model,
        attachments,
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
    let provider = options
        .provider
        .as_ref()
        .map(|selection| selection.as_str().to_owned());
    let model = options.model.clone().or_else(|| {
        options
            .provider
            .as_ref()
            .is_some_and(ProviderSelection::is_fake)
            .then(|| "fake-model".into())
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
    let mut attachments = Vec::with_capacity(options.attachments.len());
    for path in &options.attachments {
        // G2: image sniff first (existing behavior); a readable non-image
        // falls back to the UTF-8 text-file lane — the SAME fallback the
        // TUI's `/attach` performs. Any other failure (unreadable, over the
        // cap, non-UTF-8 binary) reports the honest loader error.
        let loaded = match load_image_attachment(path) {
            Ok(image) => Ok(HeadlessAttachment::Image(image)),
            Err(HeadlessRunError::Attachment { ref code, .. })
                if code == "unsupported_attachment_type" =>
            {
                load_text_attachment(path).map(HeadlessAttachment::File)
            }
            Err(error) => Err(error),
        };
        match loaded {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => {
                if options.output == RunOutput::Json
                    && let Err(io_error) = write_error_json(
                        io::stdout().lock(),
                        &error,
                        provider.as_deref(),
                        model.as_deref(),
                    )
                {
                    eprintln!("haider: stdout failed: {io_error}");
                    return ExitCode::from(EX_IOERR);
                }
                eprintln!("haider: {error}");
                return ExitCode::from(exit_code_for_error(&error));
            }
        }
    }
    let request = HeadlessRunRequest {
        cwd,
        prompt: options.prompt.clone(),
        attachments,
        provider: provider.clone(),
        model: model.clone(),
        max_tokens: profile.default_max_tokens,
        permission_overrides: SessionPermissionOverridesV1 {
            allow_writes: options.allow_writes,
            allow_exec: options.allow_exec,
        },
        trust_hooks: options.trust_hooks,
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
            // W-A decision 8: the run summary NAMES still-running tasks —
            // the turn is over, the daemon keeps ownership, and they end
            // with the session (stderr, so json pipelines stay clean).
            if !result.background_tasks_running.is_empty() {
                let names = result
                    .background_tasks_running
                    .iter()
                    .map(|task| task.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "haider: note: {} background task(s) still running ({names}) — owned by the daemon session; they end when the session closes",
                    result.background_tasks_running.len(),
                );
            }
            if result.outcome != HeadlessOutcome::Done
                && let Some(failure) = &result.failure
            {
                eprintln!("haider: {}", failure.message);
                if matches!(
                    failure.code,
                    HeadlessFailureCode::Run(ErrorCode::CredentialMissing)
                ) {
                    eprintln!(
                        "haider: set HAIDER_ANTHROPIC_API_KEY, run `haider import codex`, or sign in from the TUI"
                    );
                }
            }
            ExitCode::from(exit_code_for_result(&result))
        }
        Err(error) => {
            // The v1 object is the json contract for EVERY outcome — a
            // pre-acceptance timeout or transport failure still emits one
            // (null ids: no run was accepted), never a bare stderr line.
            if options.output == RunOutput::Json
                && let Err(io_error) = write_error_json(
                    io::stdout().lock(),
                    &error,
                    provider.as_deref(),
                    model.as_deref(),
                )
            {
                eprintln!("haider: stdout failed: {io_error}");
                return ExitCode::from(EX_IOERR);
            }
            eprintln!("haider: {error}");
            if matches!(
                &error,
                HeadlessRunError::Bootstrap {
                    code: ERROR_CODE_NO_ACTIVE_ACCOUNT,
                    ..
                }
            ) {
                eprintln!(
                    "haider: remedy: sign in from the TUI or configure an active account, then retry"
                );
            }
            ExitCode::from(exit_code_for_error(&error))
        }
    }
}

/// The `haider.run.v1` object for a run that never produced a
/// [`HeadlessRunResult`] — ids are null, the outcome is `timeout` only
/// for the pre-acceptance wall-clock class, and the error carries the
/// typed code.
fn write_error_json(
    mut output: impl Write,
    error: &HeadlessRunError,
    provider: Option<&str>,
    model: Option<&str>,
) -> io::Result<()> {
    let (outcome, code, retryable) = match error {
        HeadlessRunError::Attachment { code, .. } => ("errored", code.clone(), false),
        HeadlessRunError::Rpc {
            code, retryable, ..
        } if code == "timeout_before_acceptance" => ("timeout", "timeout".to_owned(), *retryable),
        HeadlessRunError::Rpc {
            code, retryable, ..
        } => ("errored", code.clone(), *retryable),
        HeadlessRunError::Bootstrap {
            code, retryable, ..
        } => ("errored", (*code).to_owned(), *retryable),
        _ => ("errored", "internal".to_owned(), false),
    };
    let message = serde_json::to_string(&error.to_string()).map_err(io::Error::other)?;
    let code = serde_json::to_string(&code).map_err(io::Error::other)?;
    let provider = serde_json::to_string(&provider).map_err(io::Error::other)?;
    let model = serde_json::to_string(&model).map_err(io::Error::other)?;
    let line = format!(
        "{{\"schema\":\"haider.run.v1\",\"session_id\":null,\"run_id\":null,\"provider\":{provider},\"model\":{model},\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome}\",\"response\":null,\"usage\":null,\"permission_denials\":[],\"error\":{{\"code\":{code},\"message\":{message},\"retryable\":{retryable}}}}}"
    );
    output.write_all(line.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
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
    let provider = serde_json::to_string(&result.provider).map_err(io::Error::other)?;
    let model = serde_json::to_string(&result.model).map_err(io::Error::other)?;
    let attachment_refs = result
        .attachments
        .iter()
        .map(haider_protocol::ids::ArtifactRef::as_str)
        .collect::<Vec<_>>();
    let attachment_refs = serde_json::to_string(&attachment_refs).map_err(io::Error::other)?;
    let attachment_count = result.attachments.len();
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
    // W-A decision 8 (additive to the v1 object): tasks the daemon still
    // owns when the TURN completed — they die with the session, not the run.
    let background_tasks = serde_json::to_string(
        &result
            .background_tasks_running
            .iter()
            .map(|task| serde_json::json!({"task_id": task.task_id, "name": task.name}))
            .collect::<Vec<_>>(),
    )
    .map_err(io::Error::other)?;
    Ok(format!(
        "{{\"schema\":\"haider.run.v1\",\"session_id\":{session_id},\"run_id\":{run_id},\"provider\":{provider},\"model\":{model},\"attachments\":{{\"count\":{attachment_count},\"refs\":{attachment_refs}}},\"outcome\":{outcome},\"response\":{response},\"usage\":{usage},\"permission_denials\":{denials},\"background_tasks_running\":{background_tasks},\"error\":{error}}}"
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
        HeadlessRunError::Attachment { code, .. } if code == "attachment_io" => EX_IOERR,
        HeadlessRunError::Attachment { .. } => EX_USAGE,
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
        HeadlessRunError::Bootstrap {
            code: ERROR_CODE_NO_ACTIVE_ACCOUNT | ERROR_CODE_NO_DEFAULT_MODEL,
            ..
        } => EX_PROVIDER,
        HeadlessRunError::Bootstrap { .. } => EX_SOFTWARE,
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
