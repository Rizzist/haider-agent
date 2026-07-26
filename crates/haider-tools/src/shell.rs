//! Composer `!` escape parsing and harness-side shell builtins.
//!
//! Builtins mutate only [`ShellSession`] state and never spawn. Other escaped
//! commands become [`ProcessExec`] values which callers execute through
//! [`EffectBroker::process_exec_user`](crate::EffectBroker::process_exec_user),
//! preserving the distinct user-typed authorization source.

use crate::{ProcessExec, ToolError, ToolResult};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerSubmission {
    Message(String),
    Builtin(BuiltinResult),
    UserProcess(ProcessExec),
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
                        value: env::var(name).ok(),
                    })
                    .collect(),
            }));
        }

        self.next_call += 1;
        Ok(ComposerSubmission::UserProcess(
            ProcessExec::new(format!("shell-{}", self.next_call), command)
                .with_cwd(self.cwd.clone())
                .with_env_allowlist(self.env_allowlist.clone()),
        ))
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
