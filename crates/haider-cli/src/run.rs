//! Manual `haider run` parser and daemon-backed output adapter.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use haider_client::{
    ConnectError, DaemonLifetime, ERROR_CODE_NO_ACTIVE_ACCOUNT, ERROR_CODE_NO_DEFAULT_MODEL,
    EnsureError, EnsureOptions, HeadlessEvent, HeadlessFailureCode, HeadlessOutcome,
    HeadlessRunError, HeadlessRunRequest, HeadlessRunResult, HeadlessSessionConfig, ProfileEnv,
    headless_run_events, headless_run_status, load_attachment, resolve_profile,
    run_headless_with_session_config, stop_headless_run,
};
use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_protocol::headless::RunBudgetV1;
use haider_protocol::ids::RunId;
use haider_protocol::session::SessionPermissionOverridesV1;
use serde::Serialize;
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
    pub prompt_stdin: bool,
    pub action: RunAction,
    pub output: RunOutput,
    pub timeout: Option<Duration>,
    pub allow_writes: bool,
    pub allow_exec: bool,
    pub auto_allow: bool,
    pub trust_hooks: bool,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub budget: RunBudgetV1,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunAction {
    Execute,
    Start,
    Status(RunId),
    Stop(RunId),
    Replay(RunId),
}

#[allow(dead_code)]
pub(crate) fn parse_run_options(rest: &[String]) -> Result<RunOptions, String> {
    parse_run_options_with_config(rest).map(|parsed| parsed.options)
}

struct ParsedRunOptions {
    options: RunOptions,
    session_config: HeadlessSessionConfig,
}

fn parse_run_options_with_config(rest: &[String]) -> Result<ParsedRunOptions, String> {
    let mut output = None;
    let mut legacy_jsonl = false;
    let mut json = false;
    let mut timeout = None;
    let mut allow_writes = false;
    let mut allow_exec = false;
    let mut auto_allow = false;
    let mut trust_hooks = false;
    let mut provider = None;
    let mut model = None;
    let mut effort = None;
    let mut fast = None;
    let mut account = None;
    let mut attachments = Vec::new();
    let mut prompt = None;
    let mut prompt_stdin = false;
    let mut action = RunAction::Execute;
    let mut budget = RunBudgetV1::default();
    let mut seed = None;
    let mut index = 0;

    while index < rest.len() {
        match rest[index].as_str() {
            "--jsonl" if !legacy_jsonl => legacy_jsonl = true,
            "--jsonl" => return Err("duplicate --jsonl flag".into()),
            "--json" if !json => json = true,
            "--json" => return Err("duplicate --json flag".into()),
            "-p" if prompt.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "-p requires a prompt".to_owned())?;
                if value.is_empty() {
                    return Err("-p requires a non-empty prompt".into());
                }
                prompt = Some(value.clone());
            }
            "-p" => return Err("exactly one prompt source is required".into()),
            "--start" if action == RunAction::Execute => action = RunAction::Start,
            "--start" => return Err("only one lifecycle action may be requested".into()),
            "--status" if action == RunAction::Execute => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--status requires a run id".to_owned())?;
                action = RunAction::Status(parse_run_id(value, "--status")?);
            }
            "--status" => return Err("only one lifecycle action may be requested".into()),
            "--stop" if action == RunAction::Execute => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--stop requires a run id".to_owned())?;
                action = RunAction::Stop(parse_run_id(value, "--stop")?);
            }
            "--stop" => return Err("only one lifecycle action may be requested".into()),
            "--replay" if action == RunAction::Execute => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--replay requires a run id".to_owned())?;
                action = RunAction::Replay(parse_run_id(value, "--replay")?);
            }
            "--replay" => return Err("only one lifecycle action may be requested".into()),
            "--max-tokens" if budget.max_tokens.is_none() => {
                index += 1;
                budget.max_tokens = Some(parse_positive_u64(
                    rest.get(index).map(String::as_str),
                    "--max-tokens",
                )?);
            }
            "--max-tokens" => return Err("duplicate --max-tokens flag".into()),
            "--max-cost" if budget.max_cost_microusd.is_none() => {
                index += 1;
                budget.max_cost_microusd =
                    Some(parse_cost_microusd(rest.get(index).map(String::as_str))?);
            }
            "--max-cost" => return Err("duplicate --max-cost flag".into()),
            "--max-time" if budget.max_time_ms.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--max-time requires a duration".to_owned())?;
                budget.max_time_ms =
                    Some(u64::try_from(parse_timeout(value)?.as_millis()).unwrap_or(u64::MAX));
            }
            "--max-time" => return Err("duplicate --max-time flag".into()),
            "--seed" if seed.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--seed requires an unsigned integer".to_owned())?;
                seed = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "--seed requires an unsigned integer".to_owned())?,
                );
            }
            "--seed" => return Err("duplicate --seed flag".into()),
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
            // Full auto-allow (Codex --full-auto analogue): every effect class
            // resolves to Allow, including computer control and web fetch.
            "--auto-allow" if !auto_allow => auto_allow = true,
            "--auto-allow" => return Err("duplicate --auto-allow flag".into()),
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
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| "--model requires a model id".to_owned())?;
                model = Some(value.clone());
            }
            "--model" => return Err("duplicate --model flag".into()),
            "--effort" if effort.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| "--effort requires a level".to_owned())?;
                effort = Some(value.clone());
            }
            "--effort" => return Err("duplicate --effort flag".into()),
            "--speed" if fast.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| "--speed requires fast|normal".to_owned())?;
                fast = Some(match value.as_str() {
                    "fast" => true,
                    "normal" => false,
                    _ => return Err("--speed requires fast|normal".into()),
                });
            }
            "--speed" => return Err("duplicate --speed flag".into()),
            "--account" if account.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| "--account requires an alias".to_owned())?;
                account = Some(value.clone());
            }
            "--account" => return Err("duplicate --account flag".into()),
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
            "-" if prompt.is_none() => {
                prompt = Some(String::new());
                prompt_stdin = true;
            }
            value if prompt.is_none() => prompt = Some(value.to_owned()),
            _ => return Err("exactly one prompt argument is required".into()),
        }
        index += 1;
    }

    let output = match (legacy_jsonl, json, output) {
        (true, false, Some(RunOutput::Jsonl) | None) => RunOutput::Jsonl,
        (false, true, Some(RunOutput::Json) | None) => RunOutput::Json,
        (false, false, Some(output)) => output,
        (false, false, None) if matches!(action, RunAction::Replay(_)) => RunOutput::Json,
        (false, false, None) => RunOutput::Print,
        _ => return Err("--json/--jsonl conflicts with the selected --output".into()),
    };
    if matches!(action, RunAction::Replay(_)) && output != RunOutput::Json {
        return Err("replay requires --json output so divergence is never hidden".into());
    }
    let prompt_required = matches!(action, RunAction::Execute | RunAction::Start);
    if prompt_required && prompt.is_none() {
        return Err("a prompt source is required (-p TEXT, -, or one positional argument)".into());
    }
    if !prompt_required && prompt.is_some() {
        return Err("status, stop, and replay do not accept a prompt".into());
    }
    if matches!(&action, RunAction::Status(_) | RunAction::Stop(_)) && timeout.is_some() {
        return Err("status and stop do not accept --timeout".into());
    }
    if !prompt_required
        && (provider.is_some()
            || model.is_some()
            || effort.is_some()
            || fast.is_some()
            || account.is_some()
            || !attachments.is_empty()
            || !budget.is_empty()
            || seed.is_some()
            || allow_writes
            || allow_exec
            || auto_allow
            || trust_hooks)
    {
        return Err("status, stop, and replay use the run's pinned configuration".into());
    }
    let prompt = prompt.unwrap_or_default();
    let session_config = HeadlessSessionConfig {
        // `--model` is the session.create selection carried by
        // HeadlessRunRequest below. Re-applying it with session.select_model
        // would redundantly subject a custom compatible wire id to catalog
        // membership validation after the session already accepted it.
        model: None,
        effort,
        fast,
        account,
    };
    Ok(ParsedRunOptions {
        options: RunOptions {
            prompt,
            prompt_stdin,
            action,
            output,
            timeout,
            allow_writes,
            allow_exec,
            auto_allow,
            trust_hooks,
            provider,
            model,
            attachments,
            budget,
            seed,
        },
        session_config,
    })
}

fn parse_positive_u64(value: Option<&str>, flag: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{flag} requires a positive integer"))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} requires a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_run_id(value: &str, flag: &str) -> Result<RunId, String> {
    if value.is_empty() || value.starts_with('-') {
        return Err(format!("{flag} requires a run id, not another flag"));
    }
    Ok(RunId::new(value))
}

fn parse_cost_microusd(value: Option<&str>) -> Result<u64, String> {
    let value = value.ok_or_else(|| "--max-cost requires a positive USD amount".to_owned())?;
    let (dollars, fractional) = value.split_once('.').unwrap_or((value, ""));
    if dollars.is_empty()
        || dollars.bytes().any(|byte| !byte.is_ascii_digit())
        || fractional.len() > 6
        || fractional.bytes().any(|byte| !byte.is_ascii_digit())
    {
        return Err("--max-cost requires USD with at most six decimal places".into());
    }
    let whole = dollars
        .parse::<u64>()
        .map_err(|_| "--max-cost is too large".to_owned())?
        .checked_mul(1_000_000)
        .ok_or_else(|| "--max-cost is too large".to_owned())?;
    let padded = format!("{fractional:0<6}");
    let micros = padded
        .parse::<u64>()
        .map_err(|_| "--max-cost requires a positive USD amount".to_owned())?;
    let total = whole
        .checked_add(micros)
        .ok_or_else(|| "--max-cost is too large".to_owned())?;
    if total == 0 {
        return Err("--max-cost must be greater than zero".into());
    }
    Ok(total)
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
    let machine_output = requested_machine_output(rest);
    let parsed = match parse_run_options_with_config(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            let failure = ClassifiedRunError::bootstrap("invalid_argument", message.clone());
            if let Some(mode) = machine_output
                && let Err(error) = write_run_error(io::stdout().lock(), mode, &failure, None, None)
            {
                eprintln!("haider: stdout failed: {error}");
                return ExitCode::from(EX_IOERR);
            }
            eprintln!("haider run: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let mut options = parsed.options;
    let mut session_config = parsed.session_config;
    if options.prompt_stdin {
        match tokio::task::spawn_blocking(read_stdin_prompt).await {
            Ok(Ok(prompt)) => options.prompt = prompt,
            Ok(Err(error)) => {
                let failure = ClassifiedRunError::bootstrap("invalid_argument", error.to_string());
                if let Err(io_error) =
                    write_run_error(io::stdout().lock(), options.output, &failure, None, None)
                {
                    eprintln!("haider: stdout failed: {io_error}");
                    return ExitCode::from(EX_IOERR);
                }
                eprintln!("haider run: stdin: {error}");
                let code = match error.kind() {
                    io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => EX_USAGE,
                    _ => EX_IOERR,
                };
                return ExitCode::from(code);
            }
            Err(error) => {
                let failure = ClassifiedRunError::bootstrap("internal", error.to_string());
                if let Err(io_error) =
                    write_run_error(io::stdout().lock(), options.output, &failure, None, None)
                {
                    eprintln!("haider: stdout failed: {io_error}");
                    return ExitCode::from(EX_IOERR);
                }
                eprintln!("haider run: stdin reader failed: {error}");
                return ExitCode::from(EX_SOFTWARE);
            }
        }
    }
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            let failure = ClassifiedRunError::bootstrap("protocol_mismatch", error.to_string());
            if let Err(io_error) =
                write_run_error(io::stdout().lock(), options.output, &failure, None, None)
            {
                eprintln!("haider: stdout failed: {io_error}");
                return ExitCode::from(EX_IOERR);
            }
            eprintln!("haider: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let lifecycle_ensure = EnsureOptions {
        daemon_lifetime: DaemonLifetime::Persistent,
        ..EnsureOptions::default()
    };
    match options.action.clone() {
        RunAction::Status(run_id) => {
            return match headless_run_status(&profile, lifecycle_ensure, run_id).await {
                Ok(status) => match write_lifecycle_value(
                    io::stdout().lock(),
                    options.output,
                    "haider.run.status.v1",
                    &status,
                ) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("haider: stdout failed: {error}");
                        ExitCode::from(EX_IOERR)
                    }
                },
                Err(error) => {
                    report_lifecycle_error(options.output, "haider.run.status.v1", &error)
                }
            };
        }
        RunAction::Stop(run_id) => {
            return match stop_headless_run(&profile, lifecycle_ensure, run_id).await {
                Ok(stopped) => match write_lifecycle_value(
                    io::stdout().lock(),
                    options.output,
                    "haider.run.stop.v1",
                    &stopped,
                ) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("haider: stdout failed: {error}");
                        ExitCode::from(EX_IOERR)
                    }
                },
                Err(error) => report_lifecycle_error(options.output, "haider.run.stop.v1", &error),
            };
        }
        RunAction::Execute | RunAction::Start | RunAction::Replay(_) => {}
    }
    let mut durable_attachments = Vec::new();
    let mut replay_of = None;
    let mut replay_source_events = None;
    let mut replay_cwd = None;
    let mut replay_permissions = None;
    let mut request_max_tokens = profile.default_max_tokens;
    if let RunAction::Replay(source_run_id) = options.action.clone() {
        let replay_ensure = EnsureOptions {
            daemon_lifetime: DaemonLifetime::EphemeralIfSpawned,
            ..EnsureOptions::default()
        };
        let status =
            match headless_run_status(&profile, replay_ensure.clone(), source_run_id.clone()).await
            {
                Ok(status) => status,
                Err(error) => {
                    return report_pre_run_error(options.output, &error, None, None);
                }
            };
        if !status.state.is_terminal() || status.terminal_seq.is_none() {
            let error = HeadlessRunError::Rpc {
                stage: "headless replay",
                code: "busy".into(),
                message: "replay source is not terminal".into(),
                retryable: true,
            };
            return report_pre_run_error(options.output, &error, None, None);
        }
        if status.spec.cwd.is_empty() {
            let error = HeadlessRunError::Protocol {
                stage: "headless replay",
                message: "replay source predates the pinned workspace contract".into(),
            };
            return report_pre_run_error(options.output, &error, None, None);
        }
        let source_events = match headless_run_events(&profile, replay_ensure, &status).await {
            Ok(events) => events,
            Err(error) => {
                return report_pre_run_error(options.output, &error, None, None);
            }
        };
        let source_input = source_events.iter().find_map(|envelope| {
            if envelope.run_id.as_ref() != Some(&source_run_id) {
                return None;
            }
            match serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()? {
                EventPayload::UserMessage {
                    text, attachments, ..
                } => Some((text, attachments)),
                _ => None,
            }
        });
        let Some((prompt, attachments)) = source_input else {
            let error = HeadlessRunError::Protocol {
                stage: "headless replay",
                message: "replay source has no typed user input".into(),
            };
            return report_pre_run_error(options.output, &error, None, None);
        };
        options.prompt = prompt;
        options.provider = Some(ProviderSelection(status.spec.provider.clone()));
        options.model = Some(status.spec.model.clone());
        options.budget = status.spec.budget.clone();
        options.seed = status.spec.seed;
        options.allow_writes = status.spec.permission_overrides.allow_writes;
        options.allow_exec = status.spec.permission_overrides.allow_exec;
        options.auto_allow = status.spec.permission_overrides.auto_allow;
        replay_permissions = Some(status.spec.permission_overrides);
        options.trust_hooks = status.spec.trust_hooks;
        session_config.effort.clone_from(&status.spec.effort);
        session_config.fast = Some(status.spec.fast);
        request_max_tokens = status.spec.max_output_tokens;
        durable_attachments = attachments;
        replay_cwd = Some(status.spec.cwd.clone());
        replay_of = Some(source_run_id);
        replay_source_events = Some(source_events);
    }
    let provider = options
        .provider
        .as_ref()
        .map(|selection| selection.as_str().to_owned());
    let model = options
        .model
        .clone()
        .or_else(|| {
            options
                .provider
                .as_ref()
                .is_some_and(ProviderSelection::is_fake)
                .then(|| "fake-model".into())
        })
        .or_else(|| Some(profile.default_model.clone()));
    let cwd = match replay_cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
    }) {
        Some(cwd) => cwd,
        None => {
            let message = "current directory is unavailable or is not valid UTF-8";
            let failure = ClassifiedRunError::bootstrap("internal", message);
            if let Err(io_error) = write_run_error(
                io::stdout().lock(),
                options.output,
                &failure,
                provider.as_deref(),
                model.as_deref(),
            ) {
                eprintln!("haider: stdout failed: {io_error}");
                return ExitCode::from(EX_IOERR);
            }
            eprintln!("haider: {message}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let mut attachments = Vec::with_capacity(options.attachments.len());
    for path in &options.attachments {
        // One shared ingress order: magic-sniffed images, `.pdf` page-tree
        // admission, then strict UTF-8 files.
        let loaded = load_attachment(path);
        match loaded {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => {
                let failure = classify_headless_error(&error);
                if let Err(io_error) = write_run_error(
                    io::stdout().lock(),
                    options.output,
                    &failure,
                    provider.as_deref(),
                    model.as_deref(),
                ) {
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
        durable_attachments,
        provider: provider.clone(),
        model: model.clone(),
        max_tokens: request_max_tokens,
        budget: options.budget.clone(),
        seed: options.seed,
        replay_of,
        journal_pin: true,
        detached: options.action == RunAction::Start,
        permission_overrides: execution_permission_overrides(
            replay_permissions,
            options.allow_writes,
            options.allow_exec,
            options.auto_allow,
        ),
        trust_hooks: options.trust_hooks,
        timeout: options.timeout,
        terminal_grace: haider_client::DEFAULT_TERMINAL_GRACE,
    };

    let (events, receiver) = mpsc::channel(OUTPUT_BUFFER);
    let output_mode = options.output;
    let adapter = tokio::task::spawn_blocking(move || adapt_events(output_mode, receiver));
    let ensure = EnsureOptions {
        daemon_lifetime: if options.action == RunAction::Start {
            DaemonLifetime::Persistent
        } else {
            DaemonLifetime::EphemeralIfSpawned
        },
        ..EnsureOptions::default()
    };
    let result =
        run_headless_with_session_config(&profile, ensure, request, session_config, events).await;
    let adapter_result = match adapter.await {
        Ok(result) => result,
        Err(error) => Err(io::Error::other(format!("output adapter failed: {error}"))),
    };
    if let Err(error) = adapter_result {
        let failure = ClassifiedRunError::bootstrap("internal", error.to_string());
        if let Err(io_error) = write_run_error(
            io::stdout().lock(),
            options.output,
            &failure,
            provider.as_deref(),
            model.as_deref(),
        ) {
            eprintln!("haider: stdout failed: {io_error}");
            return ExitCode::from(EX_IOERR);
        }
        eprintln!("haider: stdout failed: {error}");
        return ExitCode::from(EX_IOERR);
    }

    match result {
        Ok(mut result) => {
            if let Some(source_events) = replay_source_events.as_ref() {
                result.replay = Some(replay_divergence(source_events, &result));
            }
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
            // W-C M2: the built-in headless attention signal — the same OSC 9
            // the TUI fires, but to stderr's tty. The daemon has no controlling
            // terminal, so the CLIENT emits it; a piped/redirected stderr gets
            // NO escape bytes (non-interactive runs emit nothing).
            emit_headless_attention(&result);
            ExitCode::from(exit_code_for_result(&result))
        }
        Err(error) => {
            // The v1 object is the json contract for EVERY outcome — a
            // pre-acceptance timeout or transport failure still emits one
            // (null ids: no run was accepted), never a bare stderr line.
            let failure = classify_headless_error(&error);
            if let Err(io_error) = write_run_error(
                io::stdout().lock(),
                options.output,
                &failure,
                provider.as_deref(),
                model.as_deref(),
            ) {
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

const MAX_STDIN_PROMPT_BYTES: usize = 1024 * 1024;

fn read_stdin_prompt() -> io::Result<String> {
    read_stdin_prompt_from(io::stdin().lock())
}

fn execution_permission_overrides(
    replay: Option<SessionPermissionOverridesV1>,
    allow_writes: bool,
    allow_exec: bool,
    auto_allow: bool,
) -> SessionPermissionOverridesV1 {
    replay.unwrap_or(SessionPermissionOverridesV1 {
        allow_writes,
        allow_exec,
        allow_mobile: false,
        auto_allow,
    })
}

pub(crate) fn read_stdin_prompt_from(mut input: impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(u64::try_from(MAX_STDIN_PROMPT_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STDIN_PROMPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt exceeds the 1 MiB stdin limit",
        ));
    }
    let mut prompt = String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "prompt stdin is not valid UTF-8",
        )
    })?;
    if prompt.ends_with('\n') {
        prompt.pop();
        if prompt.ends_with('\r') {
            prompt.pop();
        }
    }
    if prompt.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt stdin is empty",
        ));
    }
    Ok(prompt)
}

fn write_lifecycle_value(
    mut output: impl Write,
    mode: RunOutput,
    schema: &str,
    value: &impl Serialize,
) -> io::Result<()> {
    let value = serde_json::to_string(value).map_err(io::Error::other)?;
    match mode {
        RunOutput::Print => output.write_all(value.as_bytes())?,
        RunOutput::Json | RunOutput::Jsonl => {
            let schema = serde_json::to_string(schema).map_err(io::Error::other)?;
            write!(output, "{{\"schema\":{schema},\"result\":{value}}}")?;
        }
    }
    output.write_all(b"\n")?;
    output.flush()
}

fn report_pre_run_error(
    mode: RunOutput,
    error: &HeadlessRunError,
    provider: Option<&str>,
    model: Option<&str>,
) -> ExitCode {
    let failure = classify_headless_error(error);
    if let Err(io_error) = write_run_error(io::stdout().lock(), mode, &failure, provider, model) {
        eprintln!("haider: stdout failed: {io_error}");
        return ExitCode::from(EX_IOERR);
    }
    eprintln!("haider: {error}");
    ExitCode::from(exit_code_for_error(error))
}

fn report_lifecycle_error(mode: RunOutput, schema: &str, error: &HeadlessRunError) -> ExitCode {
    if mode != RunOutput::Print {
        let failure = classify_headless_error(error);
        let schema = serde_json::to_string(schema).unwrap_or_else(|_| "null".into());
        let code = serde_json::to_string(&failure.code).unwrap_or_else(|_| "null".into());
        let message = serde_json::to_string(&failure.message).unwrap_or_else(|_| "null".into());
        let line = format!(
            "{{\"schema\":{schema},\"result\":null,\"error\":{{\"code\":{code},\"message\":{message},\"retryable\":{}}}}}\n",
            failure.retryable
        );
        let mut stdout = io::stdout().lock();
        if let Err(io_error) = stdout
            .write_all(line.as_bytes())
            .and_then(|()| stdout.flush())
        {
            eprintln!("haider: stdout failed: {io_error}");
            return ExitCode::from(EX_IOERR);
        }
    }
    eprintln!("haider: {error}");
    ExitCode::from(exit_code_for_error(error))
}

fn replay_final_text(events: &[haider_protocol::envelope::RawEnvelope]) -> Option<String> {
    events
        .iter()
        .filter_map(|envelope| {
            match serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()? {
                EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::AgentMessage { text },
                    ..
                })
                | EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::IncompleteAgentMessage { text, .. },
                    ..
                }) => Some(text),
                _ => None,
            }
        })
        .last()
}

fn replay_tool_trace(events: &[haider_protocol::envelope::RawEnvelope]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|envelope| {
            let payload = &envelope.payload;
            match payload.get("type").and_then(serde_json::Value::as_str) {
                Some("tool_result") => Some(serde_json::json!({
                    "type": "tool_result",
                    "call_id": payload.get("call_id"),
                    "result": payload.get("result"),
                })),
                Some("item")
                    if payload.get("event").and_then(serde_json::Value::as_str)
                        == Some("completed")
                        && payload
                            .get("item")
                            .and_then(|item| item.get("item"))
                            .and_then(serde_json::Value::as_str)
                            == Some("tool_call") =>
                {
                    let item = payload.get("item")?;
                    Some(serde_json::json!({
                        "type": "tool_call",
                        "call_id": item.get("call_id"),
                        "name": item.get("name"),
                        "args": item.get("args"),
                        "status": item.get("status"),
                    }))
                }
                _ => None,
            }
        })
        .collect()
}

fn replay_usage_trace(events: &[haider_protocol::envelope::RawEnvelope]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|envelope| {
            let EventPayload::Usage(usage) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            let mut value = serde_json::to_value(usage).ok()?;
            normalize_replay_usage_coordinates(&mut value);
            Some(value)
        })
        .collect()
}

fn normalize_replay_usage_coordinates(usage: &mut serde_json::Value) {
    fn normalize_scope(scope: &mut serde_json::Value) {
        let Some(scope) = scope.as_object_mut() else {
            return;
        };
        if scope.contains_key("run") {
            scope.insert("run".into(), serde_json::Value::String("<run>".into()));
        }
        if scope.contains_key("agent") {
            scope.insert("agent".into(), serde_json::Value::String("<agent>".into()));
        }
    }

    if let Some(scope) = usage.get_mut("scope") {
        normalize_scope(scope);
    }
    if let Some(accounts) = usage
        .get_mut("accounts")
        .and_then(|value| value.as_array_mut())
    {
        for account in accounts {
            if let Some(scope) = account.get_mut("scope") {
                normalize_scope(scope);
            }
        }
    }
}

fn replay_terminal(events: &[haider_protocol::envelope::RawEnvelope]) -> Vec<serde_json::Value> {
    let budget = events.iter().rev().find_map(|envelope| {
        (envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("run_budget_exhausted"))
        .then(|| envelope.payload.clone())
    });
    let failure = events.iter().rev().find_map(|envelope| {
        (envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("run_failed"))
        .then(|| envelope.payload.clone())
    });
    let terminal = events.iter().rev().find_map(|envelope| {
        let payload = &envelope.payload;
        (payload.get("type").and_then(serde_json::Value::as_str) == Some("run_state")
            && matches!(
                payload.get("state").and_then(serde_json::Value::as_str),
                Some("done" | "errored" | "cancelled")
            ))
        .then(|| payload.clone())
    });
    budget.into_iter().chain(failure).chain(terminal).collect()
}

fn replay_divergence(
    source: &[haider_protocol::envelope::RawEnvelope],
    replay: &HeadlessRunResult,
) -> haider_protocol::headless::ReplayDivergenceV1 {
    let source_run_id = source
        .iter()
        .find_map(|envelope| {
            (envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str)
                == Some("headless_run_configured"))
            .then(|| envelope.run_id.clone())
            .flatten()
        })
        .unwrap_or_else(|| RunId::new("unknown"));
    let final_text_matches = replay_final_text(source) == replay.response;
    let tool_trace_matches = replay_tool_trace(source) == replay_tool_trace(&replay.events);
    let usage_matches = replay_usage_trace(source) == replay_usage_trace(&replay.events);
    let terminal_matches = replay_terminal(source) == replay_terminal(&replay.events);
    haider_protocol::headless::ReplayDivergenceV1 {
        source_run_id,
        replay_run_id: replay.run_id.clone(),
        final_text_matches,
        tool_trace_matches,
        usage_matches,
        terminal_matches,
        diverged: !(final_text_matches && tool_trace_matches && usage_matches && terminal_matches),
    }
}

fn requested_machine_output(rest: &[String]) -> Option<RunOutput> {
    if rest.iter().any(|argument| argument == "--jsonl")
        || rest
            .windows(2)
            .any(|arguments| arguments[0] == "--output" && arguments[1] == "jsonl")
    {
        Some(RunOutput::Jsonl)
    } else if rest.iter().any(|argument| argument == "--json")
        || rest
            .windows(2)
            .any(|arguments| arguments[0] == "--output" && arguments[1] == "json")
    {
        Some(RunOutput::Json)
    } else {
        None
    }
}

struct ClassifiedRunError {
    outcome: &'static str,
    code: String,
    message: String,
    retryable: bool,
}

impl ClassifiedRunError {
    fn bootstrap(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            outcome: "errored",
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
}

fn classify_headless_error(error: &HeadlessRunError) -> ClassifiedRunError {
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
        HeadlessRunError::Ensure(EnsureError::MissingFeatures { .. }) => {
            ("errored", "missing_feature".to_owned(), false)
        }
        HeadlessRunError::Ensure(
            EnsureError::ProtocolMismatch(_) | EnsureError::ProfileMismatch { .. },
        )
        | HeadlessRunError::Protocol { .. } => ("errored", "protocol_mismatch".to_owned(), false),
        _ => ("errored", "internal".to_owned(), false),
    };
    ClassifiedRunError {
        outcome,
        code,
        message: error.to_string(),
        retryable,
    }
}

#[derive(Serialize)]
struct JsonlErrorRecord<'a> {
    event: &'static str,
    stage: &'static str,
    outcome: &'static str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    error: JsonlErrorBody<'a>,
}

#[derive(Serialize)]
struct JsonlErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    retryable: bool,
}

fn write_run_error(
    mut output: impl Write,
    mode: RunOutput,
    failure: &ClassifiedRunError,
    provider: Option<&str>,
    model: Option<&str>,
) -> io::Result<()> {
    match mode {
        RunOutput::Print => return Ok(()),
        RunOutput::Json => write_error_json(&mut output, failure, provider, model)?,
        RunOutput::Jsonl => {
            serde_json::to_writer(
                &mut output,
                &JsonlErrorRecord {
                    event: "error",
                    stage: "bootstrap",
                    outcome: failure.outcome,
                    provider,
                    model,
                    error: JsonlErrorBody {
                        code: &failure.code,
                        message: &failure.message,
                        retryable: failure.retryable,
                    },
                },
            )
            .map_err(io::Error::other)?;
            output.write_all(b"\n")?;
        }
    }
    output.flush()
}

/// The `haider.run.v1` object for a run that never produced a
/// [`HeadlessRunResult`] — ids are null, the outcome is `timeout` only
/// for the pre-acceptance wall-clock class, and the error carries the
/// typed code.
fn write_error_json(
    mut output: impl Write,
    failure: &ClassifiedRunError,
    provider: Option<&str>,
    model: Option<&str>,
) -> io::Result<()> {
    let message = serde_json::to_string(&failure.message).map_err(io::Error::other)?;
    let code = serde_json::to_string(&failure.code).map_err(io::Error::other)?;
    let provider = serde_json::to_string(&provider).map_err(io::Error::other)?;
    let model = serde_json::to_string(&model).map_err(io::Error::other)?;
    let outcome = failure.outcome;
    let retryable = failure.retryable;
    let line = format!(
        "{{\"schema\":\"haider.run.v1\",\"session_id\":null,\"run_id\":null,\"provider\":{provider},\"model\":{model},\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome}\",\"response\":null,\"events\":[],\"usage\":null,\"budget_exhausted\":null,\"replay\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":{{\"code\":{code},\"message\":{message},\"retryable\":{retryable}}}}}"
    );
    output.write_all(line.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

#[derive(Serialize)]
struct AcceptedAnnouncement<'a> {
    event: &'static str,
    session_id: &'a str,
    head_seq: u64,
}

fn adapt_events(output: RunOutput, events: mpsc::Receiver<HeadlessEvent>) -> io::Result<()> {
    let stdout = io::stdout();
    let stderr = io::stderr();
    adapt_events_to(
        output,
        events,
        io::BufWriter::new(stdout.lock()),
        stderr.lock(),
    )
}

fn adapt_events_to(
    output: RunOutput,
    mut events: mpsc::Receiver<HeadlessEvent>,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> io::Result<()> {
    let mut announced = false;
    while let Some(event) = events.blocking_recv() {
        match event {
            HeadlessEvent::Accepted {
                session_id,
                head_seq,
            } if !announced => {
                let accepted = AcceptedAnnouncement {
                    event: "accepted",
                    session_id: session_id.as_str(),
                    head_seq,
                };
                match output {
                    RunOutput::Jsonl => {
                        serde_json::to_writer(&mut stdout, &accepted).map_err(io::Error::other)?;
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                    RunOutput::Json => {
                        // Single-JSON stdout remains exactly one document; the
                        // accepted record therefore uses stderr and is flushed
                        // before any model envelope can be observed.
                        serde_json::to_writer(&mut stderr, &accepted).map_err(io::Error::other)?;
                        stderr.write_all(b"\n")?;
                        stderr.flush()?;
                    }
                    RunOutput::Print => {
                        // Keep stdout as assistant-text-only for shell pipelines.
                        // The human announcement is still the first output line.
                        writeln!(stderr, "session {}", session_id.as_str())?;
                        stderr.flush()?;
                    }
                }
                announced = true;
            }
            HeadlessEvent::Accepted { .. } => {}
            HeadlessEvent::Envelope(envelope) => {
                if output == RunOutput::Jsonl {
                    serde_json::to_writer(&mut stdout, envelope.as_ref())
                        .map_err(io::Error::other)?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
            }
            HeadlessEvent::PermissionDenied(denial) => {
                writeln!(
                    stderr,
                    "haider: denied permission: {}",
                    denial.effect_summary
                )?;
                stderr.flush()?;
            }
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
            if result.outcome == HeadlessOutcome::Started {
                output.write_all(result.run_id.as_str().as_bytes())?;
                output.write_all(b"\n")?;
            } else if let Some(response) = &result.response {
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
    let events = serde_json::to_string(&result.events).map_err(io::Error::other)?;
    let budget_exhausted =
        serde_json::to_string(&result.budget_exhausted).map_err(io::Error::other)?;
    let replay = serde_json::to_string(&result.replay).map_err(io::Error::other)?;
    let denials = serde_json::to_string(&result.permission_denials).map_err(io::Error::other)?;
    let error = match &result.failure {
        Some(failure) => {
            let presentation = failure
                .presentation
                .as_ref()
                .map(|presentation| {
                    serde_json::to_string(presentation)
                        .map(|value| format!(",\"presentation\":{value}"))
                })
                .transpose()
                .map_err(io::Error::other)?
                .unwrap_or_default();
            format!(
                "{{\"code\":{},\"message\":{},\"retryable\":{}{}}}",
                serde_json::to_string(failure.code.as_str()).map_err(io::Error::other)?,
                serde_json::to_string(&failure.message).map_err(io::Error::other)?,
                failure.retryable,
                presentation,
            )
        }
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
        "{{\"schema\":\"haider.run.v1\",\"session_id\":{session_id},\"run_id\":{run_id},\"provider\":{provider},\"model\":{model},\"attachments\":{{\"count\":{attachment_count},\"refs\":{attachment_refs}}},\"outcome\":{outcome},\"response\":{response},\"events\":{events},\"usage\":{usage},\"budget_exhausted\":{budget_exhausted},\"replay\":{replay},\"permission_denials\":{denials},\"background_tasks_running\":{background_tasks},\"error\":{error}}}"
    ))
}

/// W-C M2: emit the headless desktop-notification attention signal (OSC 9) to
/// stderr — but ONLY when stderr is a real terminal. A turn that reached a
/// terminal state (Done / Errored) or the input-required attention park fires;
/// everything else stays silent. Non-interactive/piped runs emit nothing (the
/// non-tty suppression law).
fn emit_headless_attention(result: &HeadlessRunResult) {
    use haider_tui::notify::{self, Attention};
    let attention = match result.outcome {
        HeadlessOutcome::Done => Attention::Done,
        HeadlessOutcome::Errored => Attention::Errored,
        HeadlessOutcome::InputRequired => Attention::Input,
        HeadlessOutcome::Started | HeadlessOutcome::Cancelled | HeadlessOutcome::Timeout => return,
    };
    let is_tty = io::IsTerminal::is_terminal(&io::stderr());
    let line = notify::notification_line(attention, None);
    let mut err = io::stderr();
    // Non-tty stderr yields no bytes — a captured/piped run stays clean.
    let _ = err.write_all(&notify::osc9_for_tty(&line, is_tty));
    let _ = err.flush();
}

pub(crate) fn exit_code_for_result(result: &HeadlessRunResult) -> u8 {
    match result.outcome {
        HeadlessOutcome::Started | HeadlessOutcome::Done => 0,
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
                ErrorCode::PermissionDenied
                | ErrorCode::EffectUnknownOutcome
                | ErrorCode::BudgetExhausted
                | ErrorCode::WorkflowUnfinished,
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
        HeadlessRunError::Bootstrap {
            code: "missing_feature",
            ..
        } => EX_PROTOCOL,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `--model` selects the model used by session.create. It must not also
    /// schedule a redundant post-create session.select_model, whose picker
    /// validation would make a custom endpoint's catalog authoritative.
    #[test]
    fn cli_model_is_not_duplicated_into_post_create_session_config() {
        let parsed = parse_run_options_with_config(&[
            "--model".into(),
            "bench-proxy/deepseek-v4-flash".into(),
            "hello".into(),
        ])
        .expect("run options");
        assert_eq!(
            parsed.options.model.as_deref(),
            Some("bench-proxy/deepseek-v4-flash")
        );
        assert!(parsed.session_config.model.is_none());
    }

    /// Even daemon-unavailable, wire-incompatible, and no-default failures
    /// use the same terminal JSONL schema/classifier as the live command
    /// path; none may fall back to stderr-only output.
    #[test]
    fn jsonl_emitter_covers_unavailable_incompatible_and_no_default_failures() {
        let cases = [
            (
                HeadlessRunError::Ensure(EnsureError::Spawn {
                    binary: PathBuf::from("missing-haiderd"),
                    message: "daemon unavailable".into(),
                }),
                "internal",
            ),
            (
                HeadlessRunError::Ensure(EnsureError::ProtocolMismatch(
                    haider_rpc::ProtocolError {
                        code: "protocol_version_mismatch".into(),
                        message: "no wire overlap".into(),
                        fatal: true,
                        presentation: None,
                        failed_write_ids: Vec::new(),
                    },
                )),
                "protocol_mismatch",
            ),
            (
                HeadlessRunError::Bootstrap {
                    stage: "provider.list",
                    code: ERROR_CODE_NO_DEFAULT_MODEL,
                    message: "provider publishes no default model".into(),
                    retryable: false,
                },
                ERROR_CODE_NO_DEFAULT_MODEL,
            ),
        ];
        for (error, expected_code) in cases {
            let failure = classify_headless_error(&error);
            let mut output = Vec::new();
            write_run_error(&mut output, RunOutput::Jsonl, &failure, None, None)
                .expect("JSONL error");
            assert!(output.ends_with(b"\n"));
            let value: serde_json::Value =
                serde_json::from_slice(&output).expect("one JSONL object");
            assert_eq!(value["event"], "error");
            assert_eq!(value["stage"], "bootstrap");
            assert_eq!(value["error"]["code"], expected_code);
        }
    }

    #[test]
    fn replay_comparison_keeps_provider_call_ids_and_typed_failures() {
        fn envelope(
            seq: u64,
            payload: serde_json::Value,
        ) -> haider_protocol::envelope::RawEnvelope {
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "event_id": format!("event-replay-{seq}"),
                "seq": seq,
                "session_id": "session-replay",
                "run_id": "run-replay",
                "device_id": "device-replay",
                "authority_epoch": 1,
                "worker_generation": 1,
                "committed_at_ms": seq,
                "render": {"ui": true, "durable": true, "prompt": "omit"},
                "payload": payload,
            }))
            .expect("raw replay envelope")
        }

        let first_tool = vec![envelope(
            1,
            serde_json::json!({
                "type": "tool_result",
                "call_id": "provider-call-a",
                "result": {"status": "completed", "preview": "ok", "truncated": false},
            }),
        )];
        let second_tool = vec![envelope(
            1,
            serde_json::json!({
                "type": "tool_result",
                "call_id": "provider-call-b",
                "result": {"status": "completed", "preview": "ok", "truncated": false},
            }),
        )];
        assert_ne!(
            replay_tool_trace(&first_tool),
            replay_tool_trace(&second_tool)
        );

        let terminal = |message: &str| {
            vec![
                envelope(
                    2,
                    serde_json::json!({
                        "type": "run_failed",
                        "code": "provider_error",
                        "message": message,
                        "retryable": false,
                    }),
                ),
                envelope(
                    3,
                    serde_json::json!({"type": "run_state", "state": "errored"}),
                ),
            ]
        };
        assert_ne!(
            replay_terminal(&terminal("first failure")),
            replay_terminal(&terminal("different failure"))
        );

        let budget = |tokens| {
            vec![envelope(
                4,
                serde_json::json!({
                    "type": "run_budget_exhausted",
                    "dimension": "tokens",
                    "limit": 10,
                    "usage": {
                        "logical_input_tokens": tokens,
                        "billed_output_tokens": 0,
                        "additional_reasoning_tokens": 0,
                        "cache_read_tokens": 0,
                        "cache_write_tokens": 0,
                        "total_tokens": tokens,
                        "estimated_cost_microusd": 0,
                        "elapsed_ms": 1,
                    },
                }),
            )]
        };
        assert_ne!(replay_terminal(&budget(10)), replay_terminal(&budget(11)));
    }

    #[test]
    fn replay_preserves_the_complete_permission_pin() {
        let pinned = SessionPermissionOverridesV1 {
            allow_writes: false,
            allow_exec: false,
            allow_mobile: true,
            auto_allow: false,
        };
        assert_eq!(
            execution_permission_overrides(Some(pinned), true, true, true),
            pinned
        );
    }

    /// MUTATION CHECK: buffering the accepted event behind an envelope makes
    /// the first JSONL object a journal row instead of the head proof.
    #[test]
    #[allow(clippy::expect_used)]
    fn jsonl_adapter_writes_accepted_before_any_envelope() {
        let (sender, receiver) = mpsc::channel(2);
        sender
            .try_send(HeadlessEvent::Accepted {
                session_id: haider_protocol::ids::SessionId::new("session-order"),
                head_seq: 7,
            })
            .expect("accepted event queues");
        let envelope = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "event_id": "event-order",
            "seq": 7,
            "session_id": "session-order",
            "device_id": "device-order",
            "authority_epoch": 1,
            "worker_generation": 1,
            "committed_at_ms": 1,
            "render": {"ui": false, "durable": true, "prompt": "omit"},
            "payload": {"type": "future_event"}
        }))
        .expect("raw envelope fixture");
        sender
            .try_send(HeadlessEvent::Envelope(Box::new(envelope)))
            .expect("envelope queues");
        drop(sender);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        adapt_events_to(RunOutput::Jsonl, receiver, &mut stdout, &mut stderr)
            .expect("adapter succeeds");

        let lines: Vec<serde_json::Value> = String::from_utf8(stdout)
            .expect("utf8 output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL object"))
            .collect();
        assert_eq!(
            lines[0],
            serde_json::json!({
                "event": "accepted",
                "session_id": "session-order",
                "head_seq": 7
            })
        );
        assert_eq!(lines[1]["event_id"], "event-order");
        assert!(stderr.is_empty());
    }

    /// The conformance adapter consumes the daemon's existing event taxonomy
    /// directly: these exact payload shapes normalize to `retry` and
    /// `harness_error`, respectively. The CLI must not translate them into a
    /// parallel JSONL record type.
    #[test]
    fn jsonl_adapter_preserves_provider_wait_and_run_failed_payload_shapes() {
        use haider_protocol::EventPayload;
        use haider_protocol::error::ErrorCode;
        use haider_protocol::state::{RunState, WaitReason};

        fn envelope(seq: u64, payload: EventPayload) -> haider_protocol::envelope::RawEnvelope {
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "event_id": format!("event-shape-{seq}"),
                "seq": seq,
                "session_id": "session-shape",
                "device_id": "device-shape",
                "authority_epoch": 1,
                "worker_generation": 1,
                "committed_at_ms": seq,
                "run_id": "run-shape",
                "render": {"ui": true, "durable": true, "prompt": "omit"},
                "payload": serde_json::to_value(payload).expect("typed payload serializes")
            }))
            .expect("raw envelope fixture")
        }

        let (sender, receiver) = mpsc::channel(4);
        for (index, payload) in [
            EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::RateLimit,
            }),
            EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::ProviderBackoff,
            }),
            EventPayload::RunFailed {
                code: ErrorCode::ProviderError,
                message: "provider stream disconnected".into(),
                retryable: true,
                presentation: None,
            },
            EventPayload::RunFailed {
                code: ErrorCode::ProviderTimeout,
                message: "provider request timed out".into(),
                retryable: false,
                presentation: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let seq = u64::try_from(index + 1).expect("sequence fits");
            sender
                .try_send(HeadlessEvent::Envelope(Box::new(envelope(seq, payload))))
                .expect("event queues");
        }
        drop(sender);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        adapt_events_to(RunOutput::Jsonl, receiver, &mut stdout, &mut stderr)
            .expect("adapter succeeds");
        let payloads = String::from_utf8(stdout)
            .expect("utf8 output")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL object"))
            .map(|envelope| envelope["payload"].clone())
            .collect::<Vec<_>>();

        assert_eq!(
            payloads[0],
            serde_json::json!({
                "type": "run_state",
                "state": "waiting",
                "reason": {"reason": "rate_limit"}
            })
        );
        assert_eq!(
            payloads[1],
            serde_json::json!({
                "type": "run_state",
                "state": "waiting",
                "reason": {"reason": "provider_backoff"}
            })
        );
        assert_eq!(
            payloads[2],
            serde_json::json!({
                "type": "run_failed",
                "code": "provider_error",
                "message": "provider stream disconnected",
                "retryable": true
            })
        );
        assert_eq!(
            payloads[3],
            serde_json::json!({
                "type": "run_failed",
                "code": "provider_timeout",
                "message": "provider request timed out",
                "retryable": false
            })
        );
        assert!(stderr.is_empty());
    }
}
