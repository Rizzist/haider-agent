//! Manual `haider run` parser and daemon-backed output adapter.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[cfg(test)]
use haider_client::DaemonLifetime;
use haider_client::{
    ConnectError, ERROR_CODE_NO_ACTIVE_ACCOUNT, ERROR_CODE_NO_DEFAULT_MODEL, EnsureError,
    EnsureOptions, HeadlessEvent, HeadlessEventMode, HeadlessFailureCode, HeadlessInterrupt,
    HeadlessOutcome, HeadlessRunError, HeadlessRunEvents, HeadlessRunRequest, HeadlessRunResult,
    HeadlessRunStatus, HeadlessSessionConfig, HeadlessTerminalKind, ProfileEnv,
    autospawn_daemon_lifetime, headless_run_events, headless_run_status, load_attachment,
    resolve_profile, resume_headless_with_event_mode_and_interrupts,
    run_headless_with_session_config_event_mode_and_interrupts, stop_headless_run,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorCode;
use haider_protocol::headless::{HeadlessRunEventPayload, RunBudgetV1, durable_run_terminal_v1};
use haider_protocol::ids::RunId;
#[cfg(test)]
use haider_protocol::menu::Menu;
use haider_protocol::menu::{DecisionKind, MenuKind};
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
const JSONL_FLUSH_INTERVAL: Duration = Duration::from_millis(3);
const JSONL_FLUSH_ENVELOPES: usize = 8;
const DEFAULT_REPLAY_TIMEOUT: Duration = Duration::from_secs(30);

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
    pub read_only: bool,
    pub allow_writes: bool,
    pub allow_exec: bool,
    pub auto_allow: bool,
    pub trust_hooks: bool,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub budget: RunBudgetV1,
    pub resume_run_id: Option<RunId>,
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
    // Autonomous runs never wait on an Ask permission. These legacy flags
    // remain accepted no-op compatibility inputs. Their true projection
    // mirrors the public run default, but durable interaction mode is the
    // daemon-side authority that promotes every Ask class, including future
    // classes these legacy booleans do not name.
    let allow_writes = true;
    let allow_exec = true;
    let auto_allow = true;
    let mut read_only = false;
    let mut allow_writes_seen = false;
    let mut allow_exec_seen = false;
    let mut auto_allow_seen = false;
    let mut trust_hooks = false;
    let mut provider = None;
    let mut model = None;
    let mut effort = None;
    let mut fast = None;
    let mut account = None;
    let mut ssh_scope = None;
    let mut attachments = Vec::new();
    let mut prompt = None;
    let mut prompt_stdin = false;
    let mut action = RunAction::Execute;
    let mut budget = RunBudgetV1::default();
    let mut request_tranche = None;
    let mut max_requests = None;
    let mut resume_run_id = None;
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
            "--resume" if resume_run_id.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--resume requires a run id".to_owned())?;
                resume_run_id = Some(parse_run_id(value, "--resume")?);
            }
            "--resume" => return Err("duplicate --resume flag".into()),
            "--request-tranche" if request_tranche.is_none() => {
                index += 1;
                request_tranche = Some(parse_positive_u64(
                    rest.get(index).map(String::as_str),
                    "--request-tranche",
                )?);
            }
            "--request-tranche" => return Err("duplicate --request-tranche flag".into()),
            "--max-requests" if max_requests.is_none() => {
                index += 1;
                max_requests = Some(parse_positive_u64(
                    rest.get(index).map(String::as_str),
                    "--max-requests",
                )?);
            }
            "--max-requests" => return Err("duplicate --max-requests flag".into()),
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
            "--read-only" if !read_only => read_only = true,
            "--read-only" => return Err("duplicate --read-only flag".into()),
            "--allow-writes" if !allow_writes_seen => {
                allow_writes_seen = true;
            }
            "--allow-writes" => return Err("duplicate --allow-writes flag".into()),
            "--allow-exec" if !allow_exec_seen => {
                allow_exec_seen = true;
            }
            "--allow-exec" => return Err("duplicate --allow-exec flag".into()),
            "--auto-allow" if !auto_allow_seen => {
                auto_allow_seen = true;
            }
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
            "--ssh-profiles" if ssh_scope.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| "--ssh-profiles requires all|none|name[,name...]".to_owned())?;
                ssh_scope = Some(parse_ssh_scope(value)?);
            }
            "--ssh-profiles" => return Err("duplicate --ssh-profiles flag".into()),
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

    if request_tranche.is_some() || max_requests.is_some() {
        let defaults = haider_protocol::request_budget::RequestBudgetV1::default();
        let request_budget = haider_protocol::request_budget::RequestBudgetV1 {
            tranche: usize::try_from(request_tranche.unwrap_or(defaults.tranche as u64))
                .map_err(|_| "--request-tranche exceeds this platform's range")?,
            hard_cap: usize::try_from(max_requests.unwrap_or(defaults.hard_cap as u64))
                .map_err(|_| "--max-requests exceeds this platform's range")?,
        };
        request_budget.validate()?;
        budget.request_budget = Some(request_budget);
    }
    if resume_run_id.is_some() && action != RunAction::Execute {
        return Err("--resume cannot be combined with another lifecycle action".into());
    }
    if resume_run_id.is_some() {
        // New-run defaults are not caller overrides: a continuation inherits
        // its source session's permissions. Reject explicit permission flags,
        // including --read-only, which the continuation cannot reconfigure.
        if provider.is_some()
            || model.is_some()
            || effort.is_some()
            || fast.is_some()
            || account.is_some()
            || ssh_scope.is_some()
            || read_only
            || allow_writes_seen
            || allow_exec_seen
            || auto_allow_seen
            || trust_hooks
        {
            return Err("--resume inherits the source session's model and permissions".into());
        }
        if prompt.is_none() {
            prompt = Some(
                "Continue the checkpointed task using the retained messages and tool history."
                    .into(),
            );
        }
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
            || ssh_scope.is_some()
            || !attachments.is_empty()
            || !budget.is_empty()
            || seed.is_some()
            || read_only
            || allow_writes_seen
            || allow_exec_seen
            || auto_allow_seen
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
        ssh_scope,
    };
    Ok(ParsedRunOptions {
        options: RunOptions {
            prompt,
            prompt_stdin,
            resume_run_id,
            action,
            output,
            timeout,
            read_only,
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

fn parse_ssh_scope(value: &str) -> Result<haider_rpc::SshScopeWire, String> {
    match value {
        "all" => Ok(haider_rpc::SshScopeWire::All),
        "none" => Ok(haider_rpc::SshScopeWire::None),
        _ => {
            let names = value
                .split(',')
                .map(str::trim)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if names.is_empty()
                || names.iter().any(|name| {
                    name.is_empty()
                        || name.len() > 32
                        || !name.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || b"._-".contains(&byte)
                        })
                })
            {
                return Err("--ssh-profiles names must match [a-z0-9._-]{1,32}".to_owned());
            }
            Ok(haider_rpc::SshScopeWire::Allow { names })
        }
    }
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

pub(crate) fn parse_timeout(value: &str) -> Result<Duration, String> {
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
    if matches!(rest, [flag] if flag == "--help" || flag == "-h") {
        println!("{RUN_HELP}");
        return ExitCode::SUCCESS;
    }
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
    let session_config = parsed.session_config;
    let daemon_lifetime = match autospawn_daemon_lifetime(
        std::env::var_os(haider_client::AUTOSPAWN_DAEMON_IDLE_TTL_ENV).as_deref(),
    ) {
        Ok(lifetime) => lifetime,
        Err(message) => {
            let failure = ClassifiedRunError::bootstrap("invalid_argument", message.clone());
            if let Err(error) =
                write_run_error(io::stdout().lock(), options.output, &failure, None, None)
            {
                eprintln!("haider: stdout failed: {error}");
                return ExitCode::from(EX_IOERR);
            }
            eprintln!("haider run: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
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
        daemon_lifetime,
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
    if let RunAction::Replay(source_run_id) = options.action.clone() {
        let replay_ensure = EnsureOptions {
            daemon_lifetime,
            ..EnsureOptions::default()
        };
        let replay_timeout = options.timeout.unwrap_or(DEFAULT_REPLAY_TIMEOUT);
        let loaded = tokio::time::timeout(
            replay_timeout,
            load_durable_replay(&profile, replay_ensure, source_run_id),
        )
        .await;
        let (status, source_events) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                return report_lifecycle_error(options.output, "haider.run.replay.v1", &error);
            }
            Err(_) => {
                let error = HeadlessRunError::Rpc {
                    stage: "headless replay",
                    code: "replay_timeout".into(),
                    message: format!(
                        "durable replay did not finish within {} ms",
                        replay_timeout.as_millis()
                    ),
                    retryable: true,
                };
                return report_lifecycle_error(options.output, "haider.run.replay.v1", &error);
            }
        };
        return match write_durable_replay(io::stdout().lock(), &status, &source_events) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let error = HeadlessRunError::Protocol {
                    stage: "headless replay",
                    message: error.to_string(),
                };
                report_lifecycle_error(options.output, "haider.run.replay.v1", &error)
            }
        };
    }
    let provider = options
        .provider
        .as_ref()
        .map(|selection| selection.as_str().to_owned());
    let error_model = options.model.clone().or_else(|| {
        options
            .provider
            .as_ref()
            .is_some_and(ProviderSelection::is_fake)
            .then(|| "fake-model".into())
    });
    // The profile default is request input, not evidence that any session
    // bound that model. Pre-result error envelopes may carry only explicit
    // request identity; accepted results serialize the daemon's own binding.
    let request_model = error_model
        .clone()
        .or_else(|| Some(profile.default_model.clone()));
    let cwd = match std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
    {
        Some(cwd) => cwd,
        None => {
            let message = "current directory is unavailable or is not valid UTF-8";
            let failure = ClassifiedRunError::bootstrap("internal", message);
            if let Err(io_error) = write_run_error(
                io::stdout().lock(),
                options.output,
                &failure,
                provider.as_deref(),
                error_model.as_deref(),
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
                    error_model.as_deref(),
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
        durable_attachments: Vec::new(),
        provider: provider.clone(),
        model: request_model,
        max_tokens: profile.default_max_tokens,
        budget: options.budget.clone(),
        seed: options.seed,
        replay_of: None,
        journal_pin: true,
        detached: options.action == RunAction::Start,
        permission_overrides: execution_permission_overrides(
            None,
            options.read_only,
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
        daemon_lifetime,
        ..EnsureOptions::default()
    };
    let event_mode = match options.output {
        RunOutput::Jsonl => HeadlessEventMode::StreamWithoutResultLedger,
        RunOutput::Json => HeadlessEventMode::FullRecordSet,
        RunOutput::Print => HeadlessEventMode::Summary,
    };
    let (interrupt_sender, interrupt_receiver) = mpsc::unbounded_channel();
    let signal_forwarder = tokio::spawn(forward_sigints(interrupt_sender));
    let result = if let Some(source_run_id) = options.resume_run_id.clone() {
        resume_headless_with_event_mode_and_interrupts(
            &profile,
            ensure,
            request,
            source_run_id,
            events,
            event_mode,
            Some(interrupt_receiver),
        )
        .await
    } else {
        run_headless_with_session_config_event_mode_and_interrupts(
            &profile,
            ensure,
            request,
            session_config,
            events,
            event_mode,
            Some(interrupt_receiver),
        )
        .await
    };
    signal_forwarder.abort();
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
            error_model.as_deref(),
        ) {
            eprintln!("haider: stdout failed: {io_error}");
            return ExitCode::from(EX_IOERR);
        }
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
                        "haider: set HAIDER_ANTHROPIC_API_KEY, run `haider account import codex --confirm`, or sign in from the TUI"
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
                error_model.as_deref(),
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

#[cfg(unix)]
async fn forward_sigints(interrupts: mpsc::UnboundedSender<HeadlessInterrupt>) {
    let Ok(mut signals) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    else {
        return;
    };
    let mut first = true;
    while signals.recv().await.is_some() {
        let interrupt = if first {
            first = false;
            HeadlessInterrupt::CancelAndDrain
        } else {
            HeadlessInterrupt::ExitImmediately
        };
        if interrupts.send(interrupt).is_err() {
            break;
        }
    }
}

#[cfg(not(unix))]
async fn forward_sigints(interrupts: mpsc::UnboundedSender<HeadlessInterrupt>) {
    let mut first = true;
    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            break;
        }
        let interrupt = if first {
            first = false;
            HeadlessInterrupt::CancelAndDrain
        } else {
            HeadlessInterrupt::ExitImmediately
        };
        if interrupts.send(interrupt).is_err() {
            break;
        }
    }
}

const MAX_STDIN_PROMPT_BYTES: usize = 1024 * 1024;

fn read_stdin_prompt() -> io::Result<String> {
    read_stdin_prompt_from(io::stdin().lock())
}

fn execution_permission_overrides(
    replay: Option<SessionPermissionOverridesV1>,
    read_only: bool,
    allow_writes: bool,
    allow_exec: bool,
    auto_allow: bool,
) -> SessionPermissionOverridesV1 {
    replay.unwrap_or(SessionPermissionOverridesV1 {
        read_only,
        allow_writes,
        allow_exec,
        allow_mobile: false,
        auto_allow,
    })
}

const RUN_HELP: &str = "Usage: haider run (-p <prompt>|-|<prompt>) [options]\n\
\n\
Runs autonomously: Haider permission prompts are allowed automatically, including\n\
workspace writes and process execution. Explicit user deny rules, --read-only,\n\
workspace containment, and provider lockdown remain enforced.\n\
\n\
Permission options:\n\
  --read-only       Deny workspace mutation and write-capable process routes\n\
  --allow-writes    Compatibility alias; autonomous runs already allow writes\n\
  --allow-exec      Compatibility alias; autonomous runs already allow execution\n\
  --auto-allow      Compatibility alias; autonomous runs already resolve Ask to Allow\n\
  --trust-hooks     Trust configured hooks for this run\n\
\n\
Output and lifecycle options include --output print|json|jsonl, --json, --jsonl,\n\
--timeout <duration>, --start, --status, --stop, and --replay.";

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

#[derive(Serialize)]
struct DurableReplayIntegrity {
    event_count: usize,
    first_seq: u64,
    last_seq: u64,
    sequences_strictly_increasing: bool,
    run_id_stable: bool,
    exactly_one_typed_terminal: bool,
    terminal_seq_matches_status: bool,
}

#[derive(Serialize)]
struct DurableReplayEquivalence {
    definition: &'static str,
    final_text_matches: bool,
    tool_trace_matches: bool,
    usage_matches: bool,
    terminal_matches: bool,
    equivalent: bool,
}

#[derive(Serialize)]
struct DurableReplayDocument<'a> {
    schema: &'static str,
    mode: &'static str,
    session_id: &'a str,
    source_run_id: &'a str,
    terminal_seq: u64,
    provider_requests: u8,
    provider: &'a str,
    model: &'a str,
    response: Option<haider_protocol::reply::ReplyText>,
    events: &'a HeadlessRunEvents,
    provider_rounds: Vec<haider_client::provider_rounds::ProviderRound>,
    integrity: DurableReplayIntegrity,
    equivalence: DurableReplayEquivalence,
}

async fn load_durable_replay(
    profile: &haider_client::ResolvedProfile,
    ensure: EnsureOptions,
    source_run_id: RunId,
) -> Result<(HeadlessRunStatus, HeadlessRunEvents), HeadlessRunError> {
    let status = headless_run_status(profile, ensure.clone(), source_run_id).await?;
    let Some(terminal_seq) = status.terminal_seq else {
        return Err(HeadlessRunError::Rpc {
            stage: "headless replay",
            code: "busy".into(),
            message: "replay source is not terminal".into(),
            retryable: true,
        });
    };
    if !status.state.is_terminal() {
        return Err(HeadlessRunError::Rpc {
            stage: "headless replay",
            code: "busy".into(),
            message: "replay source is not terminal".into(),
            retryable: true,
        });
    }
    // `head_seq` is the session's live cursor, while `terminal_seq` is the
    // immutable run-index boundary. Same-run task facts may arrive after the
    // provider run terminal; they belong to later session observation, not to
    // the exact replay projection sealed at terminalization.
    let mut terminal_status = status;
    terminal_status.head_seq = terminal_seq;
    let events = headless_run_events(profile, ensure, &terminal_status).await?;
    Ok((terminal_status, events))
}

fn write_durable_replay(
    mut output: impl Write,
    status: &HeadlessRunStatus,
    events: &HeadlessRunEvents,
) -> io::Result<()> {
    let terminal_seq = status.terminal_seq.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal replay source omitted terminal_seq",
        )
    })?;
    // Current journals retain these fields on the terminal envelope. The
    // owned branch is only a compatibility upcast for pre-v0.0.970 rows; the
    // ordinary path serializes the journal-backed ledger directly.
    let legacy_projection = replay_legacy_terminal_projection(&status.run_id, events)?;
    let events = legacy_projection.as_ref().unwrap_or(events);
    let mut first_seq = None;
    let mut last_seq = None;
    let mut previous_seq = None;
    let mut sequences_strictly_increasing = true;
    let mut run_id_stable = true;
    let mut terminal_sequences = Vec::new();
    events.try_for_each(|envelope| {
        first_seq.get_or_insert(envelope.seq);
        if previous_seq.is_some_and(|previous| envelope.seq <= previous) {
            sequences_strictly_increasing = false;
        }
        previous_seq = Some(envelope.seq);
        last_seq = Some(envelope.seq);
        run_id_stable &= envelope.run_id.as_ref() == Some(&status.run_id);
        if is_typed_terminal_run_state(&envelope) {
            terminal_sequences.push(envelope.seq);
        }
        Ok(())
    })?;
    let exactly_one_typed_terminal = terminal_sequences.len() == 1;
    let terminal_seq_matches_status = terminal_sequences.as_slice() == [terminal_seq];
    let first_seq = first_seq.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal replay source has an empty durable event projection",
        )
    })?;
    let last_seq = last_seq.unwrap_or(first_seq);
    if !(sequences_strictly_increasing
        && run_id_stable
        && exactly_one_typed_terminal
        && terminal_seq_matches_status)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "durable replay integrity check failed",
        ));
    }
    let response = replay_final_text(events)?;
    let document = DurableReplayDocument {
        schema: "haider.run.replay.v1",
        mode: "durable_journal",
        session_id: status.session_id.as_str(),
        source_run_id: status.run_id.as_str(),
        terminal_seq,
        provider_requests: 0,
        provider: &status.spec.provider,
        model: &status.spec.model,
        response,
        events,
        provider_rounds: haider_client::provider_rounds::provider_rounds(events)?,
        integrity: DurableReplayIntegrity {
            event_count: events.len(),
            first_seq,
            last_seq,
            sequences_strictly_increasing,
            run_id_stable,
            exactly_one_typed_terminal,
            terminal_seq_matches_status,
        },
        equivalence: DurableReplayEquivalence {
            definition: "durable_run_projection_v1",
            final_text_matches: true,
            tool_trace_matches: true,
            usage_matches: true,
            terminal_matches: true,
            equivalent: true,
        },
    };
    serde_json::to_writer(&mut output, &document).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn replay_legacy_terminal_projection(
    run_id: &RunId,
    events: &HeadlessRunEvents,
) -> io::Result<Option<HeadlessRunEvents>> {
    let mut budget_exhausted = false;
    let mut deadline_exceeded = false;
    let mut request_deadline_unix_ms = None;
    let mut cancellation_intent_at_ms = None;
    let mut adjacent_failure = None::<(u64, ErrorCode)>;
    let mut blocking_error_code = None;
    let mut pending_permission_allows = HashMap::<String, (String, u32)>::new();
    let mut missing_projection = None::<(u64, &'static str, Option<&'static str>)>;

    events.try_for_each(|envelope| {
        match envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
        {
            Some("headless_run_configured") => {
                if let Some(HeadlessRunEventPayload::HeadlessRunConfigured(configured)) =
                    HeadlessRunEventPayload::from_payload_value(&envelope.payload)
                {
                    request_deadline_unix_ms = configured.request_deadline_unix_ms;
                }
                adjacent_failure = None;
            }
            Some("run_budget_exhausted") => {
                budget_exhausted = true;
                adjacent_failure = None;
            }
            Some("run_deadline_exceeded") => {
                if blocking_error_code.is_none() && cancellation_intent_at_ms.is_none() {
                    deadline_exceeded = true;
                }
                adjacent_failure = None;
            }
            Some("run_failed") => {
                adjacent_failure = match envelope.payload.decode_event() {
                    Ok(EventPayload::RunFailed { code, .. }) => Some((envelope.seq, code)),
                    _ => None,
                };
            }
            Some("menu_opened") => {
                if !deadline_exceeded
                    && cancellation_intent_at_ms.is_none()
                    && blocking_error_code.is_none()
                    && let Ok(EventPayload::MenuOpened(menu)) = envelope.payload.decode_event()
                {
                    if let MenuKind::Permission { .. } = menu.kind {
                        let allow_once = menu
                            .options
                            .iter()
                            .enumerate()
                            .find(|(_, option)| option.decision == Some(DecisionKind::AllowOnce))
                            .and_then(|(index, option)| {
                                u32::try_from(index)
                                    .ok()
                                    .map(|index| (option.key.clone(), index))
                            });
                        if let Some(allow_once) = allow_once {
                            pending_permission_allows
                                .insert(menu.id.as_str().to_owned(), allow_once);
                        } else {
                            blocking_error_code.get_or_insert("permission_allow_unavailable");
                        }
                    } else if menu.blocking {
                        blocking_error_code.get_or_insert("input_required");
                    }
                }
                adjacent_failure = None;
            }
            Some("menu_answered") => {
                if let Ok(EventPayload::MenuAnswered(answer)) = envelope.payload.decode_event()
                    && let Some((option_key, option_index)) =
                        pending_permission_allows.remove(answer.menu.as_str())
                    && !deadline_exceeded
                    && cancellation_intent_at_ms.is_none()
                    && blocking_error_code.is_none()
                    && (answer.option_key.as_deref() != Some(option_key.as_str())
                        || answer.option_index != option_index)
                {
                    blocking_error_code.get_or_insert("permission_resolution_conflict");
                }
                adjacent_failure = None;
            }
            Some("menu_closed") => {
                if let Ok(EventPayload::MenuClosed { menu, .. }) = envelope.payload.decode_event()
                    && pending_permission_allows.remove(menu.as_str()).is_some()
                    && !deadline_exceeded
                    && cancellation_intent_at_ms.is_none()
                    && blocking_error_code.is_none()
                {
                    blocking_error_code.get_or_insert("permission_resolution_conflict");
                }
                adjacent_failure = None;
            }
            Some("run_state") => {
                let state = envelope.payload.decode_event();
                let Ok(EventPayload::RunState(state)) = state else {
                    adjacent_failure = None;
                    return Ok(());
                };
                match state {
                    haider_protocol::state::RunState::Cancelling => {
                        cancellation_intent_at_ms.get_or_insert(envelope.committed_at_ms);
                    }
                    haider_protocol::state::RunState::InputRequired { .. }
                        if !deadline_exceeded
                            && cancellation_intent_at_ms.is_none()
                            && blocking_error_code.is_none() =>
                    {
                        blocking_error_code.get_or_insert("input_required");
                    }
                    haider_protocol::state::RunState::EffectOutcomeUnknown
                        if !deadline_exceeded
                            && cancellation_intent_at_ms.is_none()
                            && blocking_error_code.is_none() =>
                    {
                        blocking_error_code.get_or_insert("effect_outcome_unknown");
                    }
                    _ => {}
                }
                let failure_code = adjacent_failure.take().and_then(|(failure_seq, code)| {
                    failure_seq
                        .checked_add(1)
                        .is_some_and(|expected| expected == envelope.seq)
                        .then_some(code)
                });
                let Some(terminal) = durable_run_terminal_v1(
                    state,
                    failure_code,
                    budget_exhausted,
                    deadline_exceeded
                        || (blocking_error_code.is_none()
                            && request_deadline_unix_ms.is_some_and(|deadline| {
                                cancellation_intent_at_ms.is_some_and(|intent| intent >= deadline)
                            })),
                    blocking_error_code,
                ) else {
                    return Ok(());
                };
                let error_code = terminal.error_code;
                let mut payload = envelope.payload;
                if retain_or_project_terminal_fields(
                    &mut payload,
                    terminal.terminal_kind,
                    error_code,
                )? {
                    missing_projection = Some((envelope.seq, terminal.terminal_kind, error_code));
                }
            }
            _ => adjacent_failure = None,
        }
        Ok(())
    })?;

    let Some((terminal_seq, terminal_kind, error_code)) = missing_projection else {
        return Ok(None);
    };
    let mut projected = Vec::with_capacity(events.len());
    events.try_for_each(|mut envelope| {
        if envelope.seq == terminal_seq {
            retain_or_project_terminal_fields(&mut envelope.payload, terminal_kind, error_code)?;
        }
        projected.push(envelope);
        Ok(())
    })?;
    HeadlessRunEvents::from_envelopes(run_id.clone(), projected)
        .map(Some)
        .map_err(io::Error::other)
}

/// Returns true when an old payload needed an additive compatibility upcast.
/// Retained fields are journal source-of-truth data and are never rebuilt.
/// The shared classifier only fills fields absent from a pre-v0.0.970 row.
fn retain_or_project_terminal_fields(
    payload: &mut serde_json::Value,
    terminal_kind: &str,
    error_code: Option<&str>,
) -> io::Result<bool> {
    let payload = payload.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal envelope payload is not an object",
        )
    })?;
    match payload.get("terminal_kind") {
        Some(serde_json::Value::String(_)) => {
            return match payload.get("error_code") {
                None | Some(serde_json::Value::String(_)) => Ok(false),
                Some(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "retained error_code is not a string",
                )),
            };
        }
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retained terminal_kind is not a string",
            ));
        }
        None => {
            payload.insert(
                "terminal_kind".into(),
                serde_json::Value::String(terminal_kind.into()),
            );
        }
    }
    let projected = true;
    match (payload.get("error_code"), error_code) {
        (Some(value), _) if value.is_string() => {}
        (None, Some(code)) => {
            payload.insert("error_code".into(), serde_json::Value::String(code.into()));
        }
        (None, None) => {}
        (Some(_), _) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retained error_code is not a string",
            ));
        }
    }
    Ok(projected)
}

fn terminal_kind_name(kind: HeadlessTerminalKind) -> &'static str {
    match kind {
        HeadlessTerminalKind::Success => "success",
        HeadlessTerminalKind::Failure => "failure",
        HeadlessTerminalKind::Budget => "budget",
        HeadlessTerminalKind::Cancellation => "cancellation",
        HeadlessTerminalKind::Timeout => "timeout",
        HeadlessTerminalKind::ProviderError => "provider_error",
    }
}

fn replay_final_text(
    events: &HeadlessRunEvents,
) -> io::Result<Option<haider_protocol::reply::ReplyText>> {
    let mut final_text = None;
    events.try_for_each(|envelope| {
        if envelope
            .payload
            .get("provider_purpose")
            .and_then(serde_json::Value::as_str)
            == Some("compaction")
        {
            return Ok(());
        }
        let payload = envelope.payload;
        if let Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item:
                haider_protocol::item::TurnItem::AgentMessage { text }
                | haider_protocol::item::TurnItem::IncompleteAgentMessage { text, .. },
            ..
        })) = payload.decode_event()
        {
            final_text = Some(text);
        }
        Ok(())
    })?;
    Ok(final_text)
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
        } if matches!(
            code.as_str(),
            "timeout_before_acceptance" | "replay_timeout"
        ) =>
        {
            ("timeout", "timeout".to_owned(), *retryable)
        }
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
        "{{\"schema\":\"haider.run.v1\",\"session_id\":null,\"run_id\":null,\"provider\":{provider},\"model\":{model},\"attachments\":{{\"count\":0,\"refs\":[]}},\"outcome\":\"{outcome}\",\"response\":null,\"events\":[],\"provider_rounds\":[],\"usage\":null,\"budget_exhausted\":null,\"replay\":null,\"permission_denials\":[],\"background_tasks_running\":[],\"error\":{{\"code\":{code},\"message\":{message},\"retryable\":{retryable}}}}}"
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
    let mut stdout_dirty = false;
    let mut stdout_unflushed_envelopes = 0_usize;
    let mut last_stdout_flush = Instant::now();
    let mut turn_trace = None::<(u64, Instant)>;
    let mut next_event = events.blocking_recv();
    while let Some(event) = next_event {
        let mut flush_stdout = false;
        match event {
            HeadlessEvent::Accepted {
                session_id,
                head_seq,
            } if !announced => {
                if let Some((operation_micros, unix_micros)) =
                    haider_client::take_spawn_ready_trace()
                {
                    writeln!(
                        stderr,
                        "haider: trace level=TRACE target=haider.lifecycle phase=spawn_ready \
                         operation_micros={operation_micros} unix_micros={unix_micros}"
                    )?;
                    let accepted_unix_micros = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros();
                    let ready_to_accept_micros = accepted_unix_micros.saturating_sub(unix_micros);
                    writeln!(
                        stderr,
                        "haider: trace level=TRACE target=haider.lifecycle \
                         phase=client_accepted_seen operation_micros={ready_to_accept_micros} \
                         unix_micros={accepted_unix_micros}"
                    )?;
                    stderr.flush()?;
                }
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
                        stdout_dirty = false;
                        stdout_unflushed_envelopes = 0;
                        last_stdout_flush = Instant::now();
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
            HeadlessEvent::Accepted {
                session_id,
                head_seq,
            } => {
                // The first Accepted announces the newly-created session;
                // this second coordinate is the durable turn acceptance
                // returned by TurnSubmit/HeadlessRunStart. Correlate the
                // client terminal with the daemon's accepted_seq, not the
                // provisional Created sequence.
                if client_turn_trace_enabled() {
                    turn_trace = Some((
                        haider_core::turn_trace_ordinal(&session_id, head_seq),
                        Instant::now(),
                    ));
                }
            }
            HeadlessEvent::Envelope(envelope) => {
                if output == RunOutput::Jsonl {
                    haider_protocol::envelope::write_envelope_json(&mut stdout, envelope.as_ref())?;
                    stdout.write_all(b"\n")?;
                    stdout_dirty = true;
                    stdout_unflushed_envelopes = stdout_unflushed_envelopes.saturating_add(1);
                    flush_stdout = is_terminal_run_state(envelope.as_ref())
                        || stdout_unflushed_envelopes >= JSONL_FLUSH_ENVELOPES
                        || last_stdout_flush.elapsed() >= JSONL_FLUSH_INTERVAL;
                }
            }
            HeadlessEvent::Terminal(terminal) => {
                if let Some((turn_ordinal, accepted_at)) = turn_trace {
                    let end_us = accepted_at.elapsed().as_micros();
                    let unix_micros = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros();
                    writeln!(
                        stderr,
                        "haider: trace level=TRACE target=haider.turn phase=client_terminal_seen \
                         operation_micros={end_us} turn_ordinal={turn_ordinal} request_ordinal=0 \
                         txn_ordinal=0 start_us_from_accept=0 end_us_from_accept={end_us} \
                         unix_micros={unix_micros}"
                    )?;
                    stderr.flush()?;
                }
                if output == RunOutput::Jsonl {
                    let mut envelope = *terminal.envelope;
                    retain_or_project_terminal_fields(
                        &mut envelope.payload,
                        terminal_kind_name(terminal.kind),
                        terminal.error_code.as_deref(),
                    )?;
                    haider_protocol::envelope::write_envelope_json(&mut stdout, &envelope)?;
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

        if flush_stdout && stdout_dirty {
            stdout.flush()?;
            stdout_dirty = false;
            stdout_unflushed_envelopes = 0;
            last_stdout_flush = Instant::now();
        }

        next_event = match events.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::error::TryRecvError::Empty) => {
                if stdout_dirty {
                    stdout.flush()?;
                    stdout_dirty = false;
                    stdout_unflushed_envelopes = 0;
                    last_stdout_flush = Instant::now();
                }
                events.blocking_recv()
            }
            Err(mpsc::error::TryRecvError::Disconnected) => None,
        };
    }
    if stdout_dirty {
        stdout.flush()?;
    }
    Ok(())
}

fn client_turn_trace_enabled() -> bool {
    matches!(
        std::env::var_os("HAIDER_DAEMON_TRACE").as_deref(),
        Some(value) if value == "1"
    ) || matches!(
        std::env::var_os("HAIDER_CLIENT_LIFECYCLE_TRACE").as_deref(),
        Some(value) if value == "1"
    )
}

fn is_terminal_run_state(envelope: &RawEnvelope) -> bool {
    envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        == Some("run_state")
        && matches!(
            envelope
                .payload
                .get("state")
                .and_then(serde_json::Value::as_str),
            Some("done" | "errored" | "cancelled")
        )
}

fn is_typed_terminal_run_state(envelope: &RawEnvelope) -> bool {
    if !is_terminal_run_state(envelope) {
        return false;
    }
    let Some(kind_name) = envelope
        .payload
        .get("terminal_kind")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let kind = serde_json::from_value::<HeadlessTerminalKind>(serde_json::Value::String(
        kind_name.to_owned(),
    ));
    let error_code = envelope.payload.get("error_code");
    let error_value_is_typed = error_code.is_none_or(serde_json::Value::is_string);
    let Ok(kind) = kind else {
        // Terminal-kind additions are forward-compatible. The raw journal
        // value remains authoritative as long as its optional error field is
        // still structurally typed.
        return error_value_is_typed;
    };
    let state = envelope
        .payload
        .get("state")
        .and_then(serde_json::Value::as_str);
    let state_matches = matches!(
        (state, kind),
        (Some("done"), HeadlessTerminalKind::Success)
            | (
                Some("cancelled"),
                HeadlessTerminalKind::Failure
                    | HeadlessTerminalKind::Cancellation
                    | HeadlessTerminalKind::Timeout
            )
            | (
                Some("errored"),
                HeadlessTerminalKind::Failure
                    | HeadlessTerminalKind::Budget
                    | HeadlessTerminalKind::Timeout
                    | HeadlessTerminalKind::ProviderError
            )
    );
    let error_shape_matches = match kind {
        HeadlessTerminalKind::Success | HeadlessTerminalKind::Cancellation => error_code.is_none(),
        HeadlessTerminalKind::Failure
        | HeadlessTerminalKind::Budget
        | HeadlessTerminalKind::Timeout
        | HeadlessTerminalKind::ProviderError => {
            error_code.is_some_and(serde_json::Value::is_string)
        }
    };
    state_matches && error_shape_matches
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
                response.write_to(&mut output)?;
                output.write_all(b"\n")?;
            }
        }
        RunOutput::Json => {
            write_run_json(&mut output, result)?;
            output.write_all(b"\n")?;
        }
        RunOutput::Jsonl => {}
    }
    output.flush()
}

#[derive(Serialize)]
struct RunJsonAttachments<'a> {
    count: usize,
    refs: Vec<&'a str>,
}

#[derive(Serialize)]
struct RunJsonBackgroundTask<'a> {
    task_id: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
struct RunJsonError<'a> {
    code: &'a str,
    message: &'a str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<&'a haider_protocol::error::ErrorPresentation>,
}

#[derive(Serialize)]
struct RunJson<'a> {
    schema: &'static str,
    session_id: &'a str,
    run_id: &'a str,
    provider: &'a str,
    model: &'a str,
    attachments: RunJsonAttachments<'a>,
    outcome: HeadlessOutcome,
    response: &'a Option<haider_protocol::reply::ReplyText>,
    events: &'a HeadlessRunEvents,
    provider_rounds: Vec<haider_client::provider_rounds::ProviderRound>,
    usage: &'a Option<haider_protocol::provider::Usage>,
    budget_exhausted: &'a Option<haider_protocol::headless::RunBudgetExhaustedV1>,
    replay: &'a Option<haider_protocol::headless::ReplayDivergenceV1>,
    permission_denials: &'a [haider_client::HeadlessPermissionDenial],
    background_tasks_running: Vec<RunJsonBackgroundTask<'a>>,
    error: Option<RunJsonError<'a>>,
}

fn write_run_json(mut output: impl Write, result: &HeadlessRunResult) -> io::Result<()> {
    let attachment_refs = result
        .attachments
        .iter()
        .map(haider_protocol::ids::ArtifactRef::as_str)
        .collect::<Vec<_>>();
    let background_tasks_running = result
        .background_tasks_running
        .iter()
        .map(|task| RunJsonBackgroundTask {
            task_id: &task.task_id,
            name: &task.name,
        })
        .collect();
    let error = result.failure.as_ref().map(|failure| RunJsonError {
        code: failure.code.as_str(),
        message: &failure.message,
        retryable: failure.retryable,
        presentation: failure.presentation.as_ref(),
    });
    serde_json::to_writer(
        &mut output,
        &RunJson {
            schema: "haider.run.v1",
            session_id: result.session_id.as_str(),
            run_id: result.run_id.as_str(),
            provider: &result.provider,
            model: &result.model,
            attachments: RunJsonAttachments {
                count: result.attachments.len(),
                refs: attachment_refs,
            },
            outcome: result.outcome,
            response: &result.response,
            events: &result.events,
            provider_rounds: haider_client::provider_rounds::provider_rounds(&result.events)?,
            usage: &result.usage,
            budget_exhausted: &result.budget_exhausted,
            replay: &result.replay,
            permission_denials: &result.permission_denials,
            background_tasks_running,
            error,
        },
    )
    .map_err(io::Error::other)
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
                | ErrorCode::RequestBudgetExceeded
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
        HeadlessRunError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "timeout_before_acceptance" | "replay_timeout"
            ) =>
        {
            EX_TIMEOUT
        }
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
        HeadlessRunError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "request_budget_exceeded"
                    | "continuation_active"
                    | "continuation_unavailable"
                    | "continuation_scope_unsupported"
            ) =>
        {
            EX_BLOCKED
        }
        HeadlessRunError::Rpc { .. } => EX_SOFTWARE,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn request_budget_flags_preserve_defaults_and_reject_invalid_or_duplicate_limits() {
        let parsed = parse_run_options_with_config(&[
            "-p".into(),
            "long task".into(),
            "--max-requests".into(),
            "96".into(),
        ])
        .expect("hard override");
        assert_eq!(
            parsed.options.budget.request_budget,
            Some(haider_protocol::request_budget::RequestBudgetV1 {
                tranche: 32,
                hard_cap: 96
            })
        );
        for flags in [
            vec!["--request-tranche", "0"],
            vec!["--max-requests", "0"],
            vec!["--request-tranche", "65"],
            vec!["--request-tranche", "40", "--max-requests", "39"],
            vec!["--request-tranche", "32", "--request-tranche", "32"],
            vec!["--max-requests", "64", "--max-requests", "64"],
        ] {
            let mut args = vec!["-p".to_owned(), "task".to_owned()];
            args.extend(flags.iter().map(|flag| (*flag).to_owned()));
            assert!(parse_run_options_with_config(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn resume_parser_accepts_handle_and_budget_without_prompt() {
        let parsed = parse_run_options_with_config(&[
            "--resume".into(),
            "bound-run".into(),
            "--request-tranche".into(),
            "40".into(),
            "--max-requests".into(),
            "80".into(),
        ])
        .expect("continuation options");
        assert_eq!(parsed.options.resume_run_id, Some(RunId::new("bound-run")));
        assert!(!parsed.options.prompt.is_empty());
        assert_eq!(
            parsed.options.budget.request_budget,
            Some(haider_protocol::request_budget::RequestBudgetV1 {
                tranche: 40,
                hard_cap: 80
            })
        );
        for incompatible in [
            "--start",
            "--allow-writes",
            "--allow-exec",
            "--auto-allow",
            "--trust-hooks",
        ] {
            assert!(
                parse_run_options_with_config(&[
                    "--resume".into(),
                    "bound-run".into(),
                    incompatible.into()
                ])
                .is_err(),
                "{incompatible}"
            );
        }
    }

    #[test]
    fn resume_parser_accepts_defaults_and_each_budget_without_prompt() {
        for flags in [
            vec![],
            vec!["--request-tranche", "40"],
            vec!["--max-requests", "80"],
            vec!["--max-tokens", "1000"],
            vec!["--max-cost", "0.50"],
            vec!["--max-time", "10s"],
        ] {
            let mut args = vec!["--resume".to_owned(), "bound-run".to_owned()];
            args.extend(flags.iter().map(|flag| (*flag).to_owned()));
            let parsed = parse_run_options_with_config(&args).expect("continuation options");
            assert_eq!(parsed.options.resume_run_id, Some(RunId::new("bound-run")));
            assert_eq!(parsed.options.action, RunAction::Execute);
            assert_eq!(
                parsed.options.prompt,
                "Continue the checkpointed task using the retained messages and tool history."
            );
            assert!(!parsed.options.prompt_stdin);
            assert!(!parsed.options.read_only);
            assert!(parsed.options.allow_writes);
            assert!(parsed.options.allow_exec);
            assert!(parsed.options.auto_allow);
        }
    }

    #[test]
    fn resume_parser_rejects_explicit_permission_overrides_in_either_order() {
        for flag in [
            "--read-only",
            "--allow-writes",
            "--allow-exec",
            "--auto-allow",
            "--trust-hooks",
        ] {
            for args in [
                vec!["--resume", "bound-run", flag],
                vec![flag, "--resume", "bound-run"],
            ] {
                let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
                let error = parse_run_options_with_config(&args)
                    .err()
                    .expect("explicit permission override must be rejected");
                assert_eq!(
                    error, "--resume inherits the source session's model and permissions",
                    "{args:?}"
                );
            }
        }
    }

    #[test]
    fn request_budget_exit_is_blocked_and_json_preserves_resume_instruction() {
        let message = "request hard cap reached; continue with haider run --resume bound-run";
        let result = HeadlessRunResult {
            session_id: haider_protocol::ids::SessionId::new("budget-session"),
            run_id: RunId::new("bound-run"),
            provider: "fake".into(),
            model: "fake-model".into(),
            attachments: Vec::new(),
            outcome: HeadlessOutcome::Errored,
            response: None,
            usage: None,
            events: HeadlessRunEvents::empty(RunId::new("bound-run")),
            budget_exhausted: None,
            replay: None,
            permission_denials: Vec::new(),
            terminal_seq: Some(7),
            background_tasks_running: Vec::new(),
            failure: Some(haider_client::HeadlessRunFailure {
                code: HeadlessFailureCode::Run(ErrorCode::RequestBudgetExceeded),
                message: message.into(),
                retryable: false,
                presentation: None,
            }),
        };
        assert_eq!(exit_code_for_result(&result), EX_BLOCKED);
        let mut json = Vec::new();
        write_final(&mut json, RunOutput::Json, &result).expect("budget JSON");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("JSON");
        assert_eq!(value["error"]["code"], "request_budget_exceeded");
        assert_eq!(value["error"]["message"], message);
    }

    #[derive(Default)]
    struct FlushCountingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushCountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes = self.flushes.saturating_add(1);
            Ok(())
        }
    }

    #[test]
    fn run_daemon_linger_defaults_to_thirty_seconds_and_zero_restores_one_shot() {
        assert_eq!(
            autospawn_daemon_lifetime(None).expect("default linger"),
            DaemonLifetime::LingerIfSpawned {
                idle_ttl: Duration::from_secs(30),
            }
        );
        assert_eq!(
            DaemonLifetime::default(),
            DaemonLifetime::LingerIfSpawned {
                idle_ttl: Duration::from_secs(30),
            }
        );
        assert_eq!(
            autospawn_daemon_lifetime(Some(std::ffi::OsStr::new("0"))).expect("one-shot opt-out"),
            DaemonLifetime::EphemeralIfSpawned
        );
        assert_eq!(
            autospawn_daemon_lifetime(Some(std::ffi::OsStr::new("1750"))).expect("custom linger"),
            DaemonLifetime::LingerIfSpawned {
                idle_ttl: Duration::from_millis(1_750),
            }
        );
    }

    #[test]
    fn run_daemon_linger_rejects_unbounded_or_malformed_values() {
        for value in ["forever", "3600001"] {
            let error = autospawn_daemon_lifetime(Some(std::ffi::OsStr::new(value)))
                .expect_err("invalid idle TTL must be rejected");
            assert!(error.contains(haider_client::AUTOSPAWN_DAEMON_IDLE_TTL_ENV));
        }
    }

    /// `--model` selects the model used by session.create. It must not also
    /// schedule a redundant post-create session.select_model mutation after
    /// the create door has already admitted the exact pair.
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
    fn replay_preserves_the_complete_permission_pin() {
        let pinned = SessionPermissionOverridesV1 {
            read_only: false,
            allow_writes: false,
            allow_exec: false,
            allow_mobile: true,
            auto_allow: false,
        };
        assert_eq!(
            execution_permission_overrides(Some(pinned), false, true, true, true),
            pinned
        );
    }

    fn legacy_replay_envelope(seq: u64, payload: serde_json::Value) -> RawEnvelope {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "event_id": format!("event-legacy-{seq}"),
            "seq": seq,
            "session_id": "session-legacy",
            "run_id": "run-legacy",
            "device_id": "device-legacy",
            "authority_epoch": 1,
            "worker_generation": 1,
            "committed_at_ms": seq,
            "render": {"ui": true, "durable": true, "prompt": "omit"},
            "payload": payload,
        }))
        .expect("legacy raw envelope")
    }

    #[test]
    fn journalview_replay_private_summary_never_becomes_final_response() {
        let events = HeadlessRunEvents::from_envelopes(
            RunId::new("run-legacy"),
            [
                legacy_replay_envelope(
                    1,
                    serde_json::json!({
                        "type":"item", "event":"completed", "item_id":"summary",
                        "item":{"item":"agent_message", "text":"private compaction summary"},
                        "provider_purpose":"compaction",
                    }),
                ),
                legacy_replay_envelope(
                    2,
                    serde_json::json!({"type":"run_state", "state":"cancelled"}),
                ),
            ],
        )
        .expect("ledger");
        assert_eq!(replay_final_text(&events).expect("response"), None);
    }

    /// Pre-v0.0.970 journals lack the additive terminal projection. Replay
    /// upcasts those rows without mutating the journal and covers the three
    /// terminal states independently of the current-writer retention pin.
    #[test]
    fn legacy_replay_projects_success_failure_and_cancellation_terminals() {
        let cases = [
            (
                vec![legacy_replay_envelope(
                    1,
                    serde_json::json!({"type":"run_state","state":"done"}),
                )],
                "success",
                None,
            ),
            (
                vec![
                    legacy_replay_envelope(
                        1,
                        serde_json::json!({
                            "type":"run_failed",
                            "code":"internal",
                            "message":"legacy failure",
                            "retryable":false
                        }),
                    ),
                    legacy_replay_envelope(
                        2,
                        serde_json::json!({"type":"run_state","state":"errored"}),
                    ),
                ],
                "failure",
                Some("internal"),
            ),
            (
                vec![legacy_replay_envelope(
                    1,
                    serde_json::json!({"type":"run_state","state":"cancelled"}),
                )],
                "cancellation",
                None,
            ),
        ];

        for (envelopes, terminal_kind, error_code) in cases {
            let run_id = RunId::new("run-legacy");
            let events = HeadlessRunEvents::from_envelopes(run_id.clone(), envelopes)
                .expect("legacy fixture ledger");
            let projected = replay_legacy_terminal_projection(&run_id, &events)
                .expect("legacy projection")
                .expect("legacy terminal needs projection");
            let mut terminal = None;
            projected
                .try_for_each(|envelope| {
                    if is_terminal_run_state(&envelope) {
                        terminal = Some(envelope.payload);
                    }
                    Ok(())
                })
                .expect("read projected ledger");
            let terminal = terminal.expect("projected terminal");
            assert_eq!(terminal["terminal_kind"], terminal_kind);
            assert_eq!(
                terminal
                    .get("error_code")
                    .and_then(serde_json::Value::as_str),
                error_code
            );
        }
    }

    /// MUTATION CHECK: treat adjacency in the run-filtered vector as global
    /// journal adjacency. A skipped shared-session row must prevent a stale
    /// `RunFailed` from determining the terminal classifier.
    #[test]
    fn legacy_replay_requires_global_sequence_adjacency_for_run_failure() {
        let run_id = RunId::new("run-legacy");
        let events = HeadlessRunEvents::from_envelopes(
            run_id.clone(),
            vec![
                legacy_replay_envelope(
                    1,
                    serde_json::json!({
                        "type":"run_failed",
                        "code":"provider_error",
                        "message":"not adjacent",
                        "retryable":false
                    }),
                ),
                legacy_replay_envelope(
                    3,
                    serde_json::json!({"type":"run_state","state":"errored"}),
                ),
            ],
        )
        .expect("gapped legacy ledger");
        let projected = replay_legacy_terminal_projection(&run_id, &events)
            .expect("legacy projection")
            .expect("missing legacy fields");
        let mut terminal = None;
        projected
            .try_for_each(|envelope| {
                if is_terminal_run_state(&envelope) {
                    terminal = Some(envelope.payload);
                }
                Ok(())
            })
            .expect("projected ledger");
        let terminal = terminal.expect("terminal");
        assert_eq!(terminal["terminal_kind"], "failure");
        assert_eq!(terminal["error_code"], "internal");
    }

    /// MUTATION CHECK: run today's compatibility classifier over an already
    /// retained future terminal kind. Additive journal data must be served
    /// unchanged, including the absence of fields unknown to today's reader.
    #[test]
    fn legacy_replay_preserves_unknown_retained_terminal_without_synthesis() {
        let run_id = RunId::new("run-legacy");
        let terminal = legacy_replay_envelope(
            1,
            serde_json::json!({
                "type":"run_state",
                "state":"errored",
                "terminal_kind":"future_terminal"
            }),
        );
        let events = HeadlessRunEvents::from_envelopes(run_id.clone(), vec![terminal.clone()])
            .expect("future terminal ledger");
        assert!(
            replay_legacy_terminal_projection(&run_id, &events)
                .expect("compatibility scan")
                .is_none()
        );
        assert!(is_typed_terminal_run_state(&terminal));
        assert!(terminal.payload.get("error_code").is_none());
    }

    /// MUTATION CHECK: reclassify a known retained fork-boundary cancellation
    /// from preceding blocking facts. Once typed terminal fields are retained,
    /// the journal—not today's compatibility classifier—is authoritative.
    #[test]
    fn replay_preserves_known_retained_terminal_without_reclassification() {
        let run_id = RunId::new("run-legacy");
        let menu = Menu {
            id: haider_protocol::ids::MenuId::new("question"),
            kind: MenuKind::Question,
            title: "Need input".into(),
            body: Vec::new(),
            options: Vec::new(),
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "test".into(),
            ttl_ms: None,
            timeout_option: None,
        };
        let terminal = legacy_replay_envelope(
            2,
            serde_json::json!({
                "type":"run_state",
                "state":"cancelled",
                "terminal_kind":"cancellation"
            }),
        );
        let events = HeadlessRunEvents::from_envelopes(
            run_id.clone(),
            vec![
                legacy_replay_envelope(
                    1,
                    serde_json::to_value(EventPayload::MenuOpened(menu)).expect("menu"),
                ),
                terminal.clone(),
            ],
        )
        .expect("retained terminal ledger");

        assert!(
            replay_legacy_terminal_projection(&run_id, &events)
                .expect("compatibility scan")
                .is_none()
        );
        assert!(is_typed_terminal_run_state(&terminal));
        assert_eq!(terminal.payload["terminal_kind"], "cancellation");
        assert!(terminal.payload.get("error_code").is_none());
    }

    /// MUTATION CHECK: let the later effect state replace an earlier blocking
    /// menu, or map the resulting Cancelled state to ordinary cancellation.
    #[test]
    fn legacy_replay_preserves_first_blocker_on_cancelled_terminal() {
        let run_id = RunId::new("run-legacy");
        let menu = Menu {
            id: haider_protocol::ids::MenuId::new("question"),
            kind: MenuKind::Question,
            title: "Need input".into(),
            body: Vec::new(),
            options: Vec::new(),
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "test".into(),
            ttl_ms: None,
            timeout_option: None,
        };
        let events = HeadlessRunEvents::from_envelopes(
            run_id.clone(),
            vec![
                legacy_replay_envelope(
                    1,
                    serde_json::to_value(EventPayload::MenuOpened(menu)).expect("menu"),
                ),
                legacy_replay_envelope(
                    2,
                    serde_json::to_value(EventPayload::RunState(
                        haider_protocol::state::RunState::EffectOutcomeUnknown,
                    ))
                    .expect("effect state"),
                ),
                legacy_replay_envelope(
                    3,
                    serde_json::to_value(EventPayload::RunState(
                        haider_protocol::state::RunState::Cancelling,
                    ))
                    .expect("cancelling"),
                ),
                legacy_replay_envelope(
                    4,
                    serde_json::to_value(EventPayload::RunState(
                        haider_protocol::state::RunState::Cancelled,
                    ))
                    .expect("cancelled"),
                ),
            ],
        )
        .expect("blocked legacy ledger");
        let projected = replay_legacy_terminal_projection(&run_id, &events)
            .expect("legacy projection")
            .expect("missing legacy fields");
        let mut terminal = None;
        projected
            .try_for_each(|envelope| {
                if is_terminal_run_state(&envelope) {
                    terminal = Some(envelope.payload);
                }
                Ok(())
            })
            .expect("projected ledger");
        let terminal = terminal.expect("terminal");
        assert_eq!(terminal["terminal_kind"], "failure");
        assert_eq!(terminal["error_code"], "input_required");
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

    /// MUTATION CHECK: restore `flush()` after every envelope. The four-row
    /// batch then performs four flushes instead of the accepted+terminal pair.
    #[test]
    fn jsonl_adapter_flushes_a_queued_batch_at_acceptance_and_terminal() {
        fn envelope(seq: u64, state: &str) -> RawEnvelope {
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "event_id": format!("event-flush-{seq}"),
                "seq": seq,
                "session_id": "session-flush",
                "run_id": "run-flush",
                "device_id": "device-flush",
                "authority_epoch": 1,
                "worker_generation": 1,
                "committed_at_ms": seq,
                "render": {"ui": true, "durable": true, "prompt": "omit"},
                "payload": {"type": "run_state", "state": state},
            }))
            .expect("raw flush envelope")
        }

        let (sender, receiver) = mpsc::channel(4);
        sender
            .try_send(HeadlessEvent::Accepted {
                session_id: haider_protocol::ids::SessionId::new("session-flush"),
                head_seq: 1,
            })
            .expect("accepted queues");
        for (seq, state) in [(1, "thinking"), (2, "streaming"), (3, "done")] {
            sender
                .try_send(HeadlessEvent::Envelope(Box::new(envelope(seq, state))))
                .expect("envelope queues");
        }
        drop(sender);

        let mut stdout = FlushCountingWriter::default();
        let mut stderr = Vec::new();
        adapt_events_to(RunOutput::Jsonl, receiver, &mut stdout, &mut stderr)
            .expect("adapter succeeds");

        assert_eq!(
            stdout.flushes, 2,
            "accepted and terminal flush exactly once"
        );
        assert_eq!(
            stdout.bytes.iter().filter(|byte| **byte == b'\n').count(),
            4
        );
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

        let (sender, receiver) = mpsc::channel(5);
        for (index, payload) in [
            EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::NetworkUnavailable,
            }),
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
                "reason": {"reason": "network_unavailable"}
            })
        );
        assert_eq!(
            payloads[1],
            serde_json::json!({
                "type": "run_state",
                "state": "waiting",
                "reason": {"reason": "rate_limit"}
            })
        );
        assert_eq!(
            payloads[2],
            serde_json::json!({
                "type": "run_state",
                "state": "waiting",
                "reason": {"reason": "provider_backoff"}
            })
        );
        assert_eq!(
            payloads[3],
            serde_json::json!({
                "type": "run_failed",
                "code": "provider_error",
                "message": "provider stream disconnected",
                "retryable": true
            })
        );
        assert_eq!(
            payloads[4],
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
