//! Typed failures at the tool boundary.
//!
//! Blocking variants stay machine-readable so callers can act, not parse:
//! [`ToolError::AuthorizationRequired`] names the menu to answer before
//! retrying as a fresh effect, and [`ToolError::Conflict`] carries the exact
//! pre-image evidence a caller needs to rebase a stale patch.

use haider_protocol::ids::MenuId;
use std::path::PathBuf;

pub type ToolResult<T> = Result<T, ToolError>;

/// A string-replace patch could not prove that it was editing its pre-image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsPatchConflict {
    pub path: PathBuf,
    pub expected_preimage: String,
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
    Conflict(FsPatchConflict),
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
            Self::Conflict(conflict) => write!(
                formatter,
                "patch conflict for {}: expected pre-image was not present",
                conflict.path.display()
            ),
            Self::Io {
                operation,
                path,
                message,
            } => write!(formatter, "{operation} {}: {message}", path.display()),
            Self::Journal { message } => write!(formatter, "effect journal failed: {message}"),
            Self::Cas { message } => write!(formatter, "artifact storage failed: {message}"),
            Self::Runtime { message } => write!(formatter, "tool runtime failed: {message}"),
            Self::Lifecycle { message } => write!(formatter, "invalid effect lifecycle: {message}"),
        }
    }
}

impl std::error::Error for ToolError {}
