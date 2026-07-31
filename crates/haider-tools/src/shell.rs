//! Composer `!` escape parsing and harness-side shell builtins.
//!
//! Builtins mutate only [`ShellSession`] state and never spawn. Other escaped
//! commands become [`ProcessExec`] values which callers execute through
//! [`EffectBroker::process_exec_user`](crate::EffectBroker::process_exec_user),
//! preserving the distinct user-typed authorization source.

use crate::process::ProcessExec;
use crate::{ToolError, ToolResult};
use std::env;
use std::path::{Path, PathBuf};

pub const REDACTED_ENV_VALUE: &str = "•redacted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerSubmission {
    Message(String),
    Builtin(BuiltinResult),
    UserProcess(UserProcessExec),
}

/// A process command carrying unforgeable direct-composer provenance.
///
/// Public callers may receive this value from [`ShellSession::submit`], but
/// cannot construct one or turn a model-created [`ProcessExec`] into one.
///
/// ```compile_fail
/// use haider_tools::{ProcessExec, UserProcessExec};
///
/// let forged = UserProcessExec {
///     operation: ProcessExec::new("model-call", "echo forged"),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProcessExec {
    operation: ProcessExec,
    provenance: UserTypedProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserTypedProvenance(());

impl UserProcessExec {
    fn new(operation: ProcessExec) -> Self {
        Self {
            operation,
            provenance: UserTypedProvenance(()),
        }
    }

    pub(crate) fn operation(&self) -> &ProcessExec {
        &self.operation
    }

    pub(crate) fn provenance(&self) -> UserTypedProvenance {
        self.provenance
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinResult {
    ChangedDirectory { cwd: PathBuf },
    Environment { entries: Vec<EnvViewEntry> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvViewEntry {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShellSession {
    workspace_root: PathBuf,
    cwd: PathBuf,
    env_allowlist: Vec<String>,
    next_call: u64,
}

impl ShellSession {
    pub fn new(workspace_root: impl AsRef<Path>, env_allowlist: Vec<String>) -> ToolResult<Self> {
        let requested = workspace_root.as_ref();
        let workspace_root = std::fs::canonicalize(requested)
            .map_err(|error| ToolError::io("canonicalize shell workspace", requested, error))?;
        if !workspace_root.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "shell workspace is not a directory: {}",
                workspace_root.display()
            )));
        }
        let mut env_allowlist = env_allowlist;
        env_allowlist.sort();
        env_allowlist.dedup();
        if env_allowlist.iter().any(|name| name.is_empty()) {
            return Err(ToolError::invalid_argument(
                "shell env_allowlist names must not be empty",
            ));
        }
        Ok(Self {
            cwd: workspace_root.clone(),
            workspace_root,
            env_allowlist,
            next_call: 0,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Prepares one daemon-received user command without trimming or
    /// re-quoting its shell program. The caller-provided id becomes the
    /// process call id, which binds output/item receipts to the durable RPC
    /// command id. An optional cwd is workspace-relative and applies only to
    /// this invocation; it does not mutate shell or session state.
    pub fn prepare_user_process(
        &self,
        call_id: impl Into<String>,
        command: impl Into<String>,
        cwd: Option<&Path>,
    ) -> ToolResult<UserProcessExec> {
        let call_id = call_id.into();
        let command = command.into();
        if call_id.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "direct shell command id must not be empty",
            ));
        }
        if command.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "direct shell command must not be empty",
            ));
        }
        let cwd = match cwd {
            Some(cwd) => {
                if cwd.as_os_str().is_empty() || cwd.is_absolute() {
                    return Err(ToolError::invalid_argument(
                        "direct shell cwd must be a non-empty workspace-relative path",
                    ));
                }
                self.workspace_root.join(cwd)
            }
            None => self.cwd.clone(),
        };
        Ok(UserProcessExec::new(
            ProcessExec::new(call_id, command)
                .with_cwd(cwd)
                .with_env_allowlist(self.env_allowlist.clone()),
        ))
    }

    pub fn submit(&mut self, text: impl Into<String>) -> ToolResult<ComposerSubmission> {
        let text = text.into();
        let Some(escaped) = text.strip_prefix('!') else {
            return Ok(ComposerSubmission::Message(text));
        };
        let command = escaped.trim();
        if command.is_empty() {
            return Err(ToolError::invalid_argument(
                "shell escape requires a command after `!`",
            ));
        }
        if command == "cd" {
            return self.change_directory_to(self.workspace_root.clone(), ".");
        }
        if let Some(path) = command.strip_prefix("cd ") {
            return self.change_directory(path.trim());
        }
        if command == "env-view" {
            return Ok(ComposerSubmission::Builtin(BuiltinResult::Environment {
                entries: self
                    .env_allowlist
                    .iter()
                    .map(|name| EnvViewEntry {
                        name: name.clone(),
                        value: display_env_value(name, env::var(name).ok()),
                    })
                    .collect(),
            }));
        }

        self.next_call += 1;
        Ok(ComposerSubmission::UserProcess(UserProcessExec::new(
            ProcessExec::new(format!("shell-{}", self.next_call), command)
                .with_cwd(self.cwd.clone())
                .with_env_allowlist(self.env_allowlist.clone()),
        )))
    }

    fn change_directory(&mut self, path: &str) -> ToolResult<ComposerSubmission> {
        let path_buf = PathBuf::from(path);
        let requested = if path_buf.is_absolute() {
            path_buf
        } else {
            self.cwd.join(path_buf)
        };
        self.change_directory_to(requested, path)
    }

    fn change_directory_to(
        &mut self,
        requested: PathBuf,
        display_path: &str,
    ) -> ToolResult<ComposerSubmission> {
        let resolved = std::fs::canonicalize(&requested)
            .map_err(|error| ToolError::io("change shell directory", &requested, error))?;
        if !resolved.starts_with(&self.workspace_root) {
            return Err(ToolError::WorkspaceBoundary {
                workspace_root: self.workspace_root.clone(),
                requested_path: PathBuf::from(display_path),
                resolved_path: Some(resolved),
            });
        }
        if !resolved.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "shell cwd is not a directory: {}",
                resolved.display()
            )));
        }
        self.cwd = resolved.clone();
        Ok(ComposerSubmission::Builtin(
            BuiltinResult::ChangedDirectory { cwd: resolved },
        ))
    }
}

fn is_secret_env_name(name: &str) -> bool {
    const KNOWN_SECRET_NAMES: &[&str] = &[
        "PGPASSWORD",
        "MYSQL_PWD",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "NPM_TOKEN",
    ];
    const SECRET_SUBSTRINGS: &[&str] = &["PASSWORD", "PASSWD", "PWD", "PASSPHRASE"];
    const SECRET_WORDS: &[&str] = &[
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "CREDENTIALS",
        "BEARER",
    ];
    let uppercase = name.to_ascii_uppercase();
    KNOWN_SECRET_NAMES.contains(&uppercase.as_str())
        || SECRET_SUBSTRINGS
            .iter()
            .any(|secret| uppercase.contains(secret))
        || uppercase
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| SECRET_WORDS.contains(&word))
}

fn display_env_value(name: &str, value: Option<String>) -> Option<String> {
    value.map(|value| {
        if is_secret_env_name(name) {
            REDACTED_ENV_VALUE.to_owned()
        } else {
            value
        }
    })
}
