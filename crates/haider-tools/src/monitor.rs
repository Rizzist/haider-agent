//! Typed model-facing vocabulary for session monitors.
//!
//! This crate owns only strict parsing and the provider manifest. Runtime
//! registration, durable projection, source subscriptions, and wake delivery
//! belong to `haider-daemon::monitor`.

use crate::{EffectOperation, ToolError, ToolResult};
use haider_protocol::effect::EffectClass;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const MAX_MONITOR_ID_CHARS: usize = 96;
pub const MAX_MONITOR_FILTER_CHARS: usize = 1_024;
pub const MAX_MONITOR_FOLLOW_UP_CHARS: usize = 4_000;
const MAX_MONITOR_COMMAND_CHARS: usize = 8_192;
const MAX_MONITOR_PATH_CHARS: usize = 4_096;
const MIN_TIMER_INTERVAL_MS: u64 = 1_000;
const MIN_POLL_INTERVAL_MS: u64 = 5_000;
const MAX_MONITOR_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;
const MIN_MONITOR_TIMEOUT_MS: u64 = 100;
const MAX_MONITOR_TIMEOUT_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// One strict `monitor` tool call. A single tool keeps registry operations
/// discoverable without multiplying provider declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum MonitorRequest {
    Register {
        source: MonitorSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<MonitorFilter>,
        action: MonitorAction,
        #[serde(default)]
        occurrence: MonitorOccurrence,
        #[serde(default)]
        lifetime: MonitorLifetime,
    },
    List,
    Update {
        monitor_id: String,
        source: MonitorSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<MonitorFilter>,
        action: MonitorAction,
        #[serde(default)]
        occurrence: MonitorOccurrence,
        #[serde(default)]
        lifetime: MonitorLifetime,
    },
    Pause {
        monitor_id: String,
    },
    Resume {
        monitor_id: String,
    },
    Trigger {
        monitor_id: String,
    },
    Remove {
        monitor_id: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum MonitorRequestWire {
    Register(MonitorRegisterRequest),
    List(MonitorNoFields),
    Update(MonitorUpdateRequest),
    Pause(MonitorIdRequest),
    Resume(MonitorIdRequest),
    Trigger(MonitorIdRequest),
    Remove(MonitorRemoveRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorRegisterRequest {
    source: MonitorSource,
    #[serde(default)]
    filter: Option<MonitorFilter>,
    action: MonitorAction,
    #[serde(default)]
    occurrence: MonitorOccurrence,
    #[serde(default)]
    lifetime: MonitorLifetime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorRemoveRequest {
    monitor_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorIdRequest {
    monitor_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorUpdateRequest {
    monitor_id: String,
    source: MonitorSource,
    #[serde(default)]
    filter: Option<MonitorFilter>,
    action: MonitorAction,
    #[serde(default)]
    occurrence: MonitorOccurrence,
    #[serde(default)]
    lifetime: MonitorLifetime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorNoFields {}

impl<'de> Deserialize<'de> for MonitorRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match MonitorRequestWire::deserialize(deserializer)? {
            MonitorRequestWire::Register(request) => Self::Register {
                source: request.source,
                filter: request.filter,
                action: request.action,
                occurrence: request.occurrence,
                lifetime: request.lifetime,
            },
            MonitorRequestWire::List(MonitorNoFields {}) => Self::List,
            MonitorRequestWire::Update(request) => Self::Update {
                monitor_id: request.monitor_id,
                source: request.source,
                filter: request.filter,
                action: request.action,
                occurrence: request.occurrence,
                lifetime: request.lifetime,
            },
            MonitorRequestWire::Pause(request) => Self::Pause {
                monitor_id: request.monitor_id,
            },
            MonitorRequestWire::Resume(request) => Self::Resume {
                monitor_id: request.monitor_id,
            },
            MonitorRequestWire::Trigger(request) => Self::Trigger {
                monitor_id: request.monitor_id,
            },
            MonitorRequestWire::Remove(request) => Self::Remove {
                monitor_id: request.monitor_id,
            },
        })
    }
}

impl MonitorRequest {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid monitor arguments: {error}"))
        })?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> ToolResult<()> {
        match self {
            Self::Register {
                source,
                filter,
                action,
                lifetime,
                ..
            } => {
                source.validate()?;
                if let Some(filter) = filter {
                    filter.validate(source.kind())?;
                }
                action.validate()?;
                lifetime.validate()
            }
            Self::List => Ok(()),
            Self::Update {
                monitor_id,
                source,
                filter,
                action,
                lifetime,
                ..
            } => {
                validate_monitor_id(monitor_id)?;
                source.validate()?;
                if let Some(filter) = filter {
                    filter.validate(source.kind())?;
                }
                action.validate()?;
                lifetime.validate()
            }
            Self::Remove { monitor_id }
            | Self::Pause { monitor_id }
            | Self::Resume { monitor_id }
            | Self::Trigger { monitor_id } => validate_monitor_id(monitor_id),
        }
    }

    #[must_use]
    pub fn source(&self) -> Option<&MonitorSource> {
        match self {
            Self::Register { source, .. } | Self::Update { source, .. } => Some(source),
            Self::List
            | Self::Remove { .. }
            | Self::Pause { .. }
            | Self::Resume { .. }
            | Self::Trigger { .. } => None,
        }
    }
}

fn validate_monitor_id(monitor_id: &str) -> ToolResult<()> {
    let length = monitor_id.chars().count();
    if monitor_id.trim().is_empty() || length > MAX_MONITOR_ID_CHARS {
        return Err(ToolError::invalid_argument(format!(
            "monitor_id must contain 1..={MAX_MONITOR_ID_CHARS} characters"
        )));
    }
    Ok(())
}

/// Extensible source declaration. Legacy shell-command process and poll
/// shapes remain readable; CLI presets use an exact argv vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorSource {
    Sms,
    Process {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env_passthrough: Vec<String>,
        #[serde(default)]
        restart: MonitorProcessRestart,
    },
    File {
        path: String,
    },
    Poll {
        command: String,
        interval_ms: u64,
        #[serde(default)]
        until: MonitorPollUntil,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env_passthrough: Vec<String>,
    },
    Timer {
        interval_ms: u64,
    },
    Cli {
        preset: MonitorCliPreset,
        #[serde(default)]
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env_passthrough: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MonitorSourceWire {
    Sms(MonitorNoFields),
    Process(MonitorCommandSource),
    File(MonitorFileSource),
    Poll(MonitorPollSource),
    Timer(MonitorTimerSource),
    Cli(MonitorCliSource),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorCommandSource {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env_passthrough: Vec<String>,
    #[serde(default)]
    restart: MonitorProcessRestart,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorFileSource {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorPollSource {
    command: String,
    interval_ms: u64,
    #[serde(default)]
    until: MonitorPollUntil,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env_passthrough: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorTimerSource {
    interval_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorCliSource {
    preset: MonitorCliPreset,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    env_passthrough: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    interval_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for MonitorSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match MonitorSourceWire::deserialize(deserializer)? {
            MonitorSourceWire::Sms(MonitorNoFields {}) => Self::Sms,
            MonitorSourceWire::Process(source) => Self::Process {
                command: source.command,
                cwd: source.cwd,
                env_passthrough: source.env_passthrough,
                restart: source.restart,
            },
            MonitorSourceWire::File(source) => Self::File { path: source.path },
            MonitorSourceWire::Poll(source) => Self::Poll {
                command: source.command,
                interval_ms: source.interval_ms,
                until: source.until,
                cwd: source.cwd,
                env_passthrough: source.env_passthrough,
            },
            MonitorSourceWire::Timer(source) => Self::Timer {
                interval_ms: source.interval_ms,
            },
            MonitorSourceWire::Cli(source) => Self::Cli {
                preset: source.preset,
                argv: source.argv,
                env_passthrough: source.env_passthrough,
                cwd: source.cwd,
                interval_ms: source.interval_ms,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorProcessRestart {
    #[default]
    Never,
    OnFailure,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorPollUntil {
    ExitCode {
        #[serde(default)]
        code: i32,
    },
    StdoutMatches {
        pattern: String,
        #[serde(default)]
        case_sensitive: bool,
    },
    #[default]
    StdoutChanged,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MonitorPollUntilWire {
    ExitCode(MonitorExitCodeUntil),
    StdoutMatches(MonitorStdoutMatchesUntil),
    StdoutChanged(MonitorNoFields),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorExitCodeUntil {
    #[serde(default)]
    code: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorStdoutMatchesUntil {
    pattern: String,
    #[serde(default)]
    case_sensitive: bool,
}

impl<'de> Deserialize<'de> for MonitorPollUntil {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match MonitorPollUntilWire::deserialize(deserializer)? {
            MonitorPollUntilWire::ExitCode(until) => Self::ExitCode { code: until.code },
            MonitorPollUntilWire::StdoutMatches(until) => Self::StdoutMatches {
                pattern: until.pattern,
                case_sensitive: until.case_sensitive,
            },
            MonitorPollUntilWire::StdoutChanged(MonitorNoFields {}) => Self::StdoutChanged,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MonitorCliPreset {
    Codex,
    ClaudeCode,
    Opencode,
    Antigravity,
    GhCi,
    Custom,
}

impl MonitorSource {
    #[must_use]
    pub fn kind(&self) -> MonitorSourceKind {
        match self {
            Self::Sms => MonitorSourceKind::Sms,
            Self::Process { .. } => MonitorSourceKind::Process,
            Self::File { .. } => MonitorSourceKind::File,
            Self::Poll { .. } => MonitorSourceKind::Poll,
            Self::Timer { .. } => MonitorSourceKind::Timer,
            Self::Cli { .. } => MonitorSourceKind::Cli,
        }
    }

    fn validate(&self) -> ToolResult<()> {
        match self {
            Self::Sms => Ok(()),
            Self::Process {
                command,
                cwd,
                env_passthrough,
                ..
            } => {
                bounded_nonempty(command, MAX_MONITOR_COMMAND_CHARS, "monitor command")?;
                validate_optional_path(cwd.as_deref())?;
                validate_env_passthrough(env_passthrough)?;
                Ok(())
            }
            Self::Poll {
                command,
                interval_ms,
                until,
                cwd,
                env_passthrough,
            } => {
                bounded_nonempty(command, MAX_MONITOR_COMMAND_CHARS, "monitor command")?;
                validate_poll_interval(*interval_ms)?;
                if let MonitorPollUntil::StdoutMatches { pattern, .. } = until {
                    bounded_nonempty(pattern, MAX_MONITOR_FILTER_CHARS, "stdout match pattern")?;
                }
                validate_optional_path(cwd.as_deref())?;
                validate_env_passthrough(env_passthrough)
            }
            Self::File { path } => {
                bounded_nonempty(path, MAX_MONITOR_PATH_CHARS, "monitor file path")
            }
            Self::Timer { interval_ms } => validate_timer_interval(*interval_ms),
            Self::Cli {
                preset,
                argv,
                env_passthrough,
                cwd,
                interval_ms,
            } => {
                validate_argv(argv, *preset)?;
                validate_env_passthrough(env_passthrough)?;
                validate_optional_path(cwd.as_deref())?;
                if *preset == MonitorCliPreset::GhCi {
                    validate_poll_interval(interval_ms.unwrap_or(MIN_POLL_INTERVAL_MS))?;
                } else if interval_ms.is_some() {
                    return Err(ToolError::invalid_argument(
                        "monitor cli interval_ms is only valid for the gh-ci preset",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Exact argv executed by a command-backed source. Legacy command sources
    /// intentionally retain shell semantics by making the shell itself
    /// explicit in the approved vector.
    #[must_use]
    pub fn resolved_argv(&self) -> Option<Vec<String>> {
        match self {
            Self::Process { command, .. } | Self::Poll { command, .. } => {
                Some(crate::process::monitor_shell_argv(command))
            }
            Self::Cli { preset, argv, .. } => Some(cli_preset_argv(*preset, argv)),
            Self::Sms | Self::File { .. } | Self::Timer { .. } => None,
        }
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        match self {
            Self::Process { cwd, .. } | Self::Poll { cwd, .. } | Self::Cli { cwd, .. } => {
                cwd.as_deref()
            }
            Self::Sms | Self::File { .. } | Self::Timer { .. } => None,
        }
    }

    #[must_use]
    pub fn env_passthrough(&self) -> &[String] {
        match self {
            Self::Process {
                env_passthrough, ..
            }
            | Self::Poll {
                env_passthrough, ..
            }
            | Self::Cli {
                env_passthrough, ..
            } => env_passthrough,
            Self::Sms | Self::File { .. } | Self::Timer { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorSourceKind {
    Sms,
    Process,
    File,
    Poll,
    Timer,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorFilter {
    pub field: MonitorFilterField,
    pub operator: MonitorFilterOperator,
    pub value: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

impl MonitorFilter {
    fn validate(&self, source: MonitorSourceKind) -> ToolResult<()> {
        bounded_nonempty(
            &self.value,
            MAX_MONITOR_FILTER_CHARS,
            "monitor filter value",
        )?;
        let compatible = match source {
            MonitorSourceKind::Sms => {
                matches!(
                    self.field,
                    MonitorFilterField::Address | MonitorFilterField::Body
                )
            }
            MonitorSourceKind::Process
            | MonitorSourceKind::File
            | MonitorSourceKind::Poll
            | MonitorSourceKind::Cli => self.field == MonitorFilterField::Payload,
            MonitorSourceKind::Timer => false,
        };
        if !compatible {
            return Err(ToolError::invalid_argument(format!(
                "monitor filter field `{:?}` is not valid for source `{source:?}`",
                self.field
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorFilterField {
    Address,
    Body,
    Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorFilterOperator {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorAction {
    #[serde(default = "default_report")]
    pub report: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<String>,
}

impl MonitorAction {
    fn validate(&self) -> ToolResult<()> {
        if let Some(follow_up) = &self.follow_up {
            bounded_nonempty(follow_up, MAX_MONITOR_FOLLOW_UP_CHARS, "monitor follow_up")?;
        }
        if !self.report && self.follow_up.is_none() {
            return Err(ToolError::invalid_argument(
                "monitor action must report the event and/or provide follow_up",
            ));
        }
        Ok(())
    }
}

fn default_report() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorOccurrence {
    Once,
    #[default]
    Every,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorLifetime {
    #[default]
    Session,
    Timeout {
        timeout_ms: u64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MonitorLifetimeWire {
    Session(MonitorNoFields),
    Timeout(MonitorTimeoutLifetime),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorTimeoutLifetime {
    timeout_ms: u64,
}

impl<'de> Deserialize<'de> for MonitorLifetime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match MonitorLifetimeWire::deserialize(deserializer)? {
            MonitorLifetimeWire::Session(MonitorNoFields {}) => Self::Session,
            MonitorLifetimeWire::Timeout(lifetime) => Self::Timeout {
                timeout_ms: lifetime.timeout_ms,
            },
        })
    }
}

impl MonitorLifetime {
    fn validate(self) -> ToolResult<()> {
        let Self::Timeout { timeout_ms } = self else {
            return Ok(());
        };
        if !(MIN_MONITOR_TIMEOUT_MS..=MAX_MONITOR_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ToolError::invalid_argument(format!(
                "monitor timeout_ms must be {MIN_MONITOR_TIMEOUT_MS}..={MAX_MONITOR_TIMEOUT_MS}"
            )));
        }
        Ok(())
    }
}

fn validate_timer_interval(interval_ms: u64) -> ToolResult<()> {
    if !(MIN_TIMER_INTERVAL_MS..=MAX_MONITOR_INTERVAL_MS).contains(&interval_ms) {
        return Err(ToolError::invalid_argument(format!(
            "timer interval_ms must be {MIN_TIMER_INTERVAL_MS}..={MAX_MONITOR_INTERVAL_MS}"
        )));
    }
    Ok(())
}

fn validate_poll_interval(interval_ms: u64) -> ToolResult<()> {
    if !(MIN_POLL_INTERVAL_MS..=MAX_MONITOR_INTERVAL_MS).contains(&interval_ms) {
        return Err(ToolError::invalid_argument(format!(
            "poll interval_ms must be {MIN_POLL_INTERVAL_MS}..={MAX_MONITOR_INTERVAL_MS}"
        )));
    }
    Ok(())
}

fn validate_optional_path(path: Option<&str>) -> ToolResult<()> {
    match path {
        Some(path) => bounded_nonempty(path, MAX_MONITOR_PATH_CHARS, "monitor cwd"),
        None => Ok(()),
    }
}

fn validate_env_passthrough(names: &[String]) -> ToolResult<()> {
    if names.len() > 64 {
        return Err(ToolError::invalid_argument(
            "monitor env_passthrough accepts at most 64 names",
        ));
    }
    for name in names {
        let mut chars = name.chars();
        let first_valid = chars
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic());
        if !first_valid
            || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(ToolError::invalid_argument(format!(
                "invalid env_passthrough name `{name}`"
            )));
        }
    }
    Ok(())
}

fn validate_argv(argv: &[String], preset: MonitorCliPreset) -> ToolResult<()> {
    if argv.len() > 128 {
        return Err(ToolError::invalid_argument(
            "monitor cli argv accepts at most 128 entries",
        ));
    }
    let total = argv.iter().try_fold(0_usize, |total, argument| {
        if argument.contains('\0') {
            return Err(ToolError::invalid_argument(
                "monitor cli argv must not contain NUL",
            ));
        }
        Ok(total.saturating_add(argument.len()))
    })?;
    if total > MAX_MONITOR_COMMAND_CHARS {
        return Err(ToolError::invalid_argument(format!(
            "monitor cli argv exceeds {MAX_MONITOR_COMMAND_CHARS} bytes"
        )));
    }
    if preset == MonitorCliPreset::Custom && argv.is_empty() {
        return Err(ToolError::invalid_argument(
            "custom monitor cli preset requires a non-empty argv",
        ));
    }
    if preset == MonitorCliPreset::GhCi && argv.is_empty() {
        return Err(ToolError::invalid_argument(
            "gh-ci monitor preset requires a run id in argv",
        ));
    }
    Ok(())
}

/// Expands only the executable/parser convention. User arguments remain
/// distinct argv elements and are never interpolated through a shell.
#[must_use]
pub fn cli_preset_argv(preset: MonitorCliPreset, arguments: &[String]) -> Vec<String> {
    let mut argv = match preset {
        MonitorCliPreset::Codex => vec!["codex".into(), "exec".into(), "--json".into()],
        MonitorCliPreset::ClaudeCode => vec![
            "claude".into(),
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
        ],
        MonitorCliPreset::Opencode => vec!["opencode".into(), "run".into()],
        MonitorCliPreset::Antigravity => vec!["antigravity".into(), "run".into()],
        MonitorCliPreset::GhCi => vec!["gh".into(), "run".into(), "view".into()],
        MonitorCliPreset::Custom => Vec::new(),
    };
    argv.extend(arguments.iter().cloned());
    if preset == MonitorCliPreset::GhCi {
        argv.extend(["--json".into(), "status,conclusion".into()]);
    }
    argv
}

/// Approval-only operation for a daemon-owned monitor runner. The effect
/// broker binds the exact argv/cwd/env-name tuple at registration time; env
/// values are deliberately absent from every argument, preview, and receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorCommandApproval {
    argv: Vec<String>,
    cwd: PathBuf,
    env_passthrough: Vec<String>,
}

impl MonitorCommandApproval {
    pub fn new(source: &MonitorSource, workspace: &Path) -> ToolResult<Option<Self>> {
        let Some(mut argv) = source.resolved_argv() else {
            return Ok(None);
        };
        let workspace = std::fs::canonicalize(workspace)
            .map_err(|error| ToolError::io("canonicalize monitor workspace", workspace, error))?;
        if argv
            .iter()
            .skip(1)
            .any(|argument| crate::redact_lockdown_text(argument) != *argument)
        {
            return Err(ToolError::invalid_argument(
                "monitor command argv must not contain credential-shaped values",
            ));
        }
        let cwd = match source.cwd() {
            Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
            Some(cwd) => workspace.join(cwd),
            None => workspace.clone(),
        };
        let cwd = std::fs::canonicalize(&cwd)
            .map_err(|error| ToolError::io("canonicalize monitor cwd", &cwd, error))?;
        if !cwd.starts_with(&workspace) {
            return Err(ToolError::WorkspaceBoundary {
                workspace_root: workspace,
                requested_path: cwd.clone(),
                resolved_path: Some(cwd),
            });
        }
        let program = argv.first().ok_or_else(|| {
            ToolError::invalid_argument("monitor command argv must contain a program")
        })?;
        let resolved_program =
            resolve_program(program, &cwd).ok_or_else(|| ToolError::Runtime {
                message: format!("monitor command binary is missing: {program}"),
            })?;
        argv[0] = resolved_program.to_string_lossy().into_owned();
        let mut env_passthrough = source.env_passthrough().to_vec();
        env_passthrough.sort();
        env_passthrough.dedup();
        Ok(Some(Self {
            argv,
            cwd,
            env_passthrough,
        }))
    }

    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn env_passthrough(&self) -> &[String] {
        &self.env_passthrough
    }
}

fn resolve_program(program: &str, cwd: &Path) -> Option<PathBuf> {
    let requested = Path::new(program);
    if requested.is_absolute() || requested.components().count() > 1 {
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            cwd.join(requested)
        };
        return resolve_program_candidate(&candidate);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .find_map(|directory| resolve_program_candidate(&directory.join(program)))
    })
}

#[cfg(not(windows))]
fn resolve_program_candidate(candidate: &Path) -> Option<PathBuf> {
    is_runnable_program(candidate)
        .then(|| std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf()))
}

#[cfg(windows)]
fn resolve_program_candidate(candidate: &Path) -> Option<PathBuf> {
    if is_runnable_program(candidate) {
        return Some(std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf()));
    }
    if candidate.extension().is_some() {
        return None;
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    resolve_program_candidate_with_extensions(candidate, &extensions)
}

#[cfg(windows)]
fn resolve_program_candidate_with_extensions(
    candidate: &Path,
    extensions: &str,
) -> Option<PathBuf> {
    extensions.split(';').find_map(|extension| {
        let extension = extension.trim();
        if extension.is_empty() {
            return None;
        }
        let mut name = candidate.as_os_str().to_os_string();
        if extension.starts_with('.') {
            name.push(extension);
        } else {
            name.push(".");
            name.push(extension);
        }
        let candidate = PathBuf::from(name);
        is_runnable_program(&candidate)
            .then(|| std::fs::canonicalize(&candidate).unwrap_or(candidate))
    })
}

#[cfg(unix)]
fn is_runnable_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_runnable_program(path: &Path) -> bool {
    path.is_file()
}

impl EffectOperation for MonitorCommandApproval {
    fn effect_class(&self) -> EffectClass {
        EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        format!("arm monitor command {}", self.argv.join(" "))
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(serde_json::json!({
            "argv": self.argv,
            "cwd": self.cwd,
            "env_passthrough": self.env_passthrough,
            "daemon_owned_monitor": true,
        }))
    }

    fn canonical_arguments(&self, _workspace_root: &Path) -> ToolResult<Value> {
        self.arguments()
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!(
                "Exact argv: {}",
                serde_json::to_string(&self.argv).unwrap_or_else(|_| "[unprintable]".into())
            ),
            format!("Working directory: {}", self.cwd.display()),
            format!("Environment names: {}", self.env_passthrough.join(", ")),
        ]
    }
}

/// The single approval operation a monitor registration may require.
/// Command sources bind ProcessExec authority; a file path that resolves
/// outside the workspace binds the same FsRead authority as a direct read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorApproval {
    Command(MonitorCommandApproval),
    ExternalFile { path: PathBuf },
}

impl MonitorApproval {
    pub fn new(source: &MonitorSource, workspace: &Path) -> ToolResult<Option<Self>> {
        if let Some(command) = MonitorCommandApproval::new(source, workspace)? {
            return Ok(Some(Self::Command(command)));
        }
        let MonitorSource::File { path } = source else {
            return Ok(None);
        };
        let workspace = std::fs::canonicalize(workspace)
            .map_err(|error| ToolError::io("canonicalize monitor workspace", workspace, error))?;
        let requested = Path::new(path);
        let requested = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace.join(requested)
        };
        let resolved = canonicalize_monitor_watch_path(&requested)?;
        if resolved.starts_with(&workspace) {
            Ok(None)
        } else {
            Ok(Some(Self::ExternalFile { path: resolved }))
        }
    }

    #[must_use]
    pub fn command(&self) -> Option<&MonitorCommandApproval> {
        match self {
            Self::Command(command) => Some(command),
            Self::ExternalFile { .. } => None,
        }
    }

    #[must_use]
    pub fn external_file(&self) -> Option<&Path> {
        match self {
            Self::Command(_) => None,
            Self::ExternalFile { path } => Some(path),
        }
    }
}

fn canonicalize_monitor_watch_path(path: &Path) -> ToolResult<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .map_err(|error| ToolError::io("canonicalize monitor file", path, error));
    }
    let name = path.file_name().ok_or_else(|| {
        ToolError::invalid_argument("external monitor file path must name a file")
    })?;
    let parent = path.parent().ok_or_else(|| {
        ToolError::invalid_argument("external monitor file path must have a parent")
    })?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| ToolError::io("canonicalize monitor file parent", parent, error))?;
    Ok(parent.join(name))
}

impl EffectOperation for MonitorApproval {
    fn effect_class(&self) -> EffectClass {
        match self {
            Self::Command(command) => command.effect_class(),
            Self::ExternalFile { .. } => EffectClass::FsRead,
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Command(command) => command.summary(),
            Self::ExternalFile { path } => format!("watch external file {}", path.display()),
        }
    }

    fn arguments(&self) -> ToolResult<Value> {
        match self {
            Self::Command(command) => command.arguments(),
            Self::ExternalFile { path } => Ok(serde_json::json!({
                "path": path,
                "daemon_owned_monitor": true,
            })),
        }
    }

    fn canonical_arguments(&self, _workspace_root: &Path) -> ToolResult<Value> {
        self.arguments()
    }

    fn approval_preview(&self) -> Vec<String> {
        match self {
            Self::Command(command) => command.approval_preview(),
            Self::ExternalFile { path } => vec![
                format!("Watch external file: {}", path.display()),
                "Read changes only; no file writes".into(),
            ],
        }
    }
}

fn bounded_nonempty(value: &str, maximum: usize, name: &str) -> ToolResult<()> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > maximum {
        return Err(ToolError::invalid_argument(format!(
            "{name} must contain 1..={maximum} characters"
        )));
    }
    Ok(())
}

pub fn monitor_manifest() -> ToolManifest {
    ToolManifest {
        name: "monitor".into(),
        description: "Register, inspect, update, pause, resume, trigger, or remove a durable session monitor. Timer, file, poll, process, CLI preset, and SMS sources wake the agent when their condition is reached.".into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["register", "list", "update", "pause", "resume", "trigger", "remove"] },
                "monitor_id": { "type": "string" },
                "source": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["sms", "process", "file", "poll", "timer", "cli"] },
                        "command": { "type": "string" },
                        "path": { "type": "string" },
                        "interval_ms": { "type": "integer" },
                        "cwd": { "type": "string" },
                        "env_passthrough": { "type": "array", "items": { "type": "string" } },
                        "restart": { "type": "string", "enum": ["never", "on_failure"] },
                        "preset": { "type": "string", "enum": ["codex", "claude-code", "opencode", "antigravity", "gh-ci", "custom"] },
                        "argv": { "type": "array", "items": { "type": "string" } },
                        "until": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["exit_code", "stdout_matches", "stdout_changed"] },
                                "code": { "type": "integer" },
                                "pattern": { "type": "string" },
                                "case_sensitive": { "type": "boolean" }
                            },
                            "required": ["kind"]
                        }
                    },
                    "required": ["kind"]
                },
                "filter": {
                    "type": "object",
                    "properties": {
                        "field": { "type": "string", "enum": ["address", "body", "payload"] },
                        "operator": { "type": "string", "enum": ["equals", "contains", "starts_with", "ends_with"] },
                        "value": { "type": "string" },
                        "case_sensitive": { "type": "boolean" }
                    },
                    "required": ["field", "operator", "value"]
                },
                "action": {
                    "type": "object",
                    "properties": {
                        "report": { "type": "boolean" },
                        "follow_up": { "type": "string" }
                    }
                },
                "occurrence": { "type": "string", "enum": ["once", "every"] },
                "lifetime": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["session", "timeout"] },
                        "timeout_ms": { "type": "integer" }
                    },
                    "required": ["kind"]
                }
            },
            "required": ["operation"]
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn strict_register_list_remove_parsing() {
        let register = MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "filter": {"field": "body", "operator": "contains", "value": "deploy"},
            "action": {"report": true, "follow_up": "summarize it"},
            "occurrence": "once",
            "lifetime": {"kind": "timeout", "timeout_ms": 1_000}
        }))
        .unwrap();
        assert!(matches!(register, MonitorRequest::Register { .. }));
        assert_eq!(
            MonitorRequest::from_tool_args(serde_json::json!({"operation": "list"})).unwrap(),
            MonitorRequest::List
        );
        assert!(matches!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "remove", "monitor_id": "monitor-1"
            }))
            .unwrap(),
            MonitorRequest::Remove { .. }
        ));
    }

    #[test]
    fn nested_and_top_level_unknown_fields_are_rejected() {
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "list", "surprise": true
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "process", "command": "printf ok", "surprise": true},
                "action": {"report": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "action": {"report": true},
                "lifetime": {"kind": "session", "surprise": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "remove", "monitor_id": "monitor-1", "surprise": true
            }))
            .is_err()
        );
    }

    #[test]
    fn every_tagged_monitor_variant_still_parses() {
        for source in [
            serde_json::json!({"kind": "sms"}),
            serde_json::json!({"kind": "process", "command": "printf ok"}),
            serde_json::json!({"kind": "file", "path": "status.txt"}),
            serde_json::json!({"kind": "poll", "command": "printf ok", "interval_ms": 5_000}),
            serde_json::json!({"kind": "timer", "interval_ms": 1_000}),
            serde_json::json!({"kind": "cli", "preset": "custom", "argv": ["printf", "ok"]}),
        ] {
            assert!(
                MonitorRequest::from_tool_args(serde_json::json!({
                    "operation": "register",
                    "source": source,
                    "action": {"report": true},
                    "lifetime": {"kind": "session"}
                }))
                .is_ok()
            );
        }
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "action": {"report": true},
                "lifetime": {"kind": "timeout", "timeout_ms": 100}
            }))
            .is_ok()
        );
    }

    #[test]
    fn source_filter_action_and_duration_cross_checks_are_enforced() {
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "filter": {"field": "payload", "operator": "contains", "value": "x"},
                "action": {"report": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "timer", "interval_ms": 999},
                "action": {"report": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "action": {"report": false}
            }))
            .is_err()
        );
    }

    #[test]
    fn cli_presets_expand_to_exact_argv_and_missing_binary_is_typed() {
        assert_eq!(
            cli_preset_argv(MonitorCliPreset::Codex, &["say hi".into()]),
            ["codex", "exec", "--json", "say hi"]
        );
        assert_eq!(
            cli_preset_argv(MonitorCliPreset::ClaudeCode, &["say hi".into()]),
            ["claude", "-p", "--output-format", "stream-json", "say hi"]
        );
        assert_eq!(
            cli_preset_argv(MonitorCliPreset::Opencode, &["say hi".into()]),
            ["opencode", "run", "say hi"]
        );
        assert_eq!(
            cli_preset_argv(MonitorCliPreset::Antigravity, &["say hi".into()]),
            ["antigravity", "run", "say hi"]
        );
        assert_eq!(
            cli_preset_argv(MonitorCliPreset::GhCi, &["123".into()]),
            ["gh", "run", "view", "123", "--json", "status,conclusion"]
        );
        assert_eq!(
            cli_preset_argv(
                MonitorCliPreset::Custom,
                &["custom-bin".into(), "--flag=value".into()]
            ),
            ["custom-bin", "--flag=value"]
        );

        let missing = MonitorSource::Cli {
            preset: MonitorCliPreset::Custom,
            argv: vec!["__not_a_real_cmd__".into()],
            env_passthrough: Vec::new(),
            cwd: None,
            interval_ms: None,
        };
        let error = MonitorCommandApproval::new(&missing, Path::new("."))
            .expect_err("missing monitor binary must be rejected before approval");
        assert!(
            error.to_string().contains("binary is missing"),
            "unexpected missing-binary error: {error}"
        );
    }

    #[test]
    fn external_file_monitor_uses_fs_read_approval_while_workspace_file_does_not() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("watched.txt");
        std::fs::write(&external, "ready").unwrap();
        let approval = MonitorApproval::new(
            &MonitorSource::File {
                path: external.to_string_lossy().into_owned(),
            },
            workspace.path(),
        )
        .unwrap()
        .expect("external file requires approval");
        assert_eq!(approval.effect_class(), EffectClass::FsRead);
        let canonical_external = std::fs::canonicalize(&external).unwrap();
        assert_eq!(approval.external_file(), Some(canonical_external.as_path()));

        assert!(
            MonitorApproval::new(
                &MonitorSource::File {
                    path: "watched.txt".into(),
                },
                workspace.path(),
            )
            .unwrap()
            .is_none()
        );

        let relative_external = PathBuf::from("..").join(
            outside
                .path()
                .file_name()
                .expect("outside temporary directory name"),
        );
        let relative_approval = MonitorApproval::new(
            &MonitorSource::File {
                path: relative_external
                    .join("watched.txt")
                    .to_string_lossy()
                    .into_owned(),
            },
            workspace.path(),
        )
        .unwrap()
        .expect("relative traversal outside the workspace requires approval");
        assert_eq!(
            relative_approval.external_file(),
            Some(canonical_external.as_path())
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_program_resolution_honors_pathext_for_exe_and_command_wrappers() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("codex.EXE");
        let wrapper = root.path().join("helper.CMD");
        std::fs::write(&executable, b"test").unwrap();
        std::fs::write(&wrapper, b"test").unwrap();

        assert_eq!(
            resolve_program_candidate_with_extensions(
                &root.path().join("codex"),
                ".COM;.EXE;.BAT;.CMD"
            ),
            Some(std::fs::canonicalize(executable).unwrap())
        );
        assert_eq!(
            resolve_program_candidate_with_extensions(
                &root.path().join("helper"),
                ".COM;.EXE;.BAT;.CMD"
            ),
            Some(std::fs::canonicalize(wrapper).unwrap())
        );
    }
}
