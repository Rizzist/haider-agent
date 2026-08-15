//! Typed failures at the tool boundary.
//!
//! Blocking variants stay machine-readable so callers can act, not parse:
//! [`ToolError::AuthorizationRequired`] names the menu to answer before
//! retrying as a fresh effect, [`ToolError::InvalidMenuAnswer`] keeps malformed
//! answers closed and retryable, [`ToolError::WorkspaceBoundary`] reports path
//! escapes, [`ToolError::PathChanged`] refuses post-authorization namespace
//! changes, [`ToolError::EditAnchor`] carries anchored replacement evidence,
//! and [`ToolError::Ledger`] makes a post-apply evidence failure explicit.

use haider_protocol::ids::MenuId;
use std::path::PathBuf;

pub type ToolResult<T> = Result<T, ToolError>;

/// An anchored edit could not identify the required number of occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEditAnchorMismatch {
    pub path: PathBuf,
    pub matches: usize,
    pub replace_all: bool,
}

/// Typed failures at the tool boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    InvalidArgument {
        message: String,
    },
    PermissionDenied {
        reason: String,
    },
    AuthorizationRequired {
        menu: MenuId,
    },
    InvalidMenuAnswer {
        menu: MenuId,
        message: String,
    },
    WorkspaceBoundary {
        workspace_root: PathBuf,
        requested_path: PathBuf,
        resolved_path: Option<PathBuf>,
    },
    PathChanged {
        path: PathBuf,
        message: String,
    },
    UnreadFile {
        path: PathBuf,
    },
    StaleRead {
        path: PathBuf,
        recorded_digest: String,
        current_digest: String,
    },
    EditAnchor(FsEditAnchorMismatch),
    Io {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
    Journal {
        message: String,
    },
    Cas {
        message: String,
    },
    Ledger {
        message: String,
    },
    Runtime {
        message: String,
    },
    Lifecycle {
        message: String,
    },
}

impl ToolError {
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }

    pub fn journal(message: impl Into<String>) -> Self {
        Self::Journal {
            message: message.into(),
        }
    }

    pub fn cas(message: impl Into<String>) -> Self {
        Self::Cas {
            message: message.into(),
        }
    }

    pub fn ledger(message: impl Into<String>) -> Self {
        Self::Ledger {
            message: message.into(),
        }
    }

    pub(crate) fn io(
        operation: &'static str,
        path: impl Into<PathBuf>,
        error: impl std::fmt::Display,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument { message } => formatter.write_str(message),
            Self::PermissionDenied { reason } => {
                write!(formatter, "effect denied by policy: {reason}")
            }
            Self::AuthorizationRequired { menu } => {
                write!(
                    formatter,
                    "effect requires authorization through menu {menu}"
                )
            }
            Self::InvalidMenuAnswer { menu, message } => {
                write!(formatter, "invalid answer for menu {menu}: {message}")
            }
            Self::WorkspaceBoundary {
                workspace_root,
                requested_path,
                resolved_path,
            } => {
                write!(
                    formatter,
                    "path {} escapes workspace root {}",
                    requested_path.display(),
                    workspace_root.display()
                )?;
                if let Some(resolved_path) = resolved_path {
                    write!(formatter, " (resolved to {})", resolved_path.display())?;
                }
                Ok(())
            }
            Self::PathChanged { path, message } => write!(
                formatter,
                "authorized path {} changed before access: {message}",
                path.display()
            ),
            Self::UnreadFile { path } => write!(
                formatter,
                "refusing to mutate unread file {}; read it before editing",
                path.display()
            ),
            Self::StaleRead { path, .. } => write!(
                formatter,
                "refusing to mutate stale file {}; re-read before editing",
                path.display()
            ),
            Self::EditAnchor(conflict) if conflict.replace_all && conflict.matches == 0 => write!(
                formatter,
                "edit anchor for {} matched 0 locations; replace_all requires at least one match",
                conflict.path.display()
            ),
            Self::EditAnchor(conflict) => write!(
                formatter,
                "edit anchor for {} matched {} locations; expected exactly 1",
                conflict.path.display(),
                conflict.matches
            ),
            Self::Io {
                operation,
                path,
                message,
            } => write!(formatter, "{operation} {}: {message}", path.display()),
            Self::Journal { message } => write!(formatter, "effect journal failed: {message}"),
            Self::Cas { message } => write!(formatter, "artifact storage failed: {message}"),
            Self::Ledger { message } => write!(formatter, "change ledger failed: {message}"),
            Self::Runtime { message } => write!(formatter, "tool runtime failed: {message}"),
            Self::Lifecycle { message } => write!(formatter, "invalid effect lifecycle: {message}"),
        }
    }
}

impl std::error::Error for ToolError {}
