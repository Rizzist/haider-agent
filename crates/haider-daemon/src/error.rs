use haider_protocol::error::HaiderError;
use std::path::PathBuf;

/// Best-effort facts about the incumbent lock holder.
///
/// These values are diagnostic only; the kernel-held profile lock remains the
/// sole authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncumbentDiagnostics {
    pub profile_id: String,
    pub endpoint_path: PathBuf,
    pub lock_contents: Option<String>,
}

#[derive(Debug)]
pub enum DaemonError {
    AlreadyRunning {
        diagnostics: IncumbentDiagnostics,
    },
    InvalidConfig {
        message: String,
    },
    Store(HaiderError),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Endpoint {
        message: String,
    },
    Protocol {
        message: String,
    },
    Task {
        message: String,
    },
}

impl DaemonError {
    pub(crate) fn io(
        operation: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::AlreadyRunning { .. } => 75,
            Self::InvalidConfig { .. } => 64,
            Self::Store(_) | Self::Io { .. } | Self::Endpoint { .. } => 74,
            Self::Protocol { .. } | Self::Task { .. } => 70,
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { diagnostics } => {
                write!(
                    formatter,
                    "profile `{}` is already running at {}",
                    diagnostics.profile_id,
                    diagnostics.endpoint_path.display()
                )?;
                if let Some(contents) = &diagnostics.lock_contents {
                    write!(
                        formatter,
                        " ({})",
                        contents.replace('\n', ", ").trim_end_matches(", ")
                    )?;
                }
                Ok(())
            }
            Self::InvalidConfig { message } => {
                write!(formatter, "invalid daemon config: {message}")
            }
            Self::Store(error) => write!(formatter, "daemon store error: {error:?}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::Endpoint { message } => write!(formatter, "daemon endpoint error: {message}"),
            Self::Protocol { message } => write!(formatter, "daemon protocol error: {message}"),
            Self::Task { message } => write!(formatter, "daemon task error: {message}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<HaiderError> for DaemonError {
    fn from(error: HaiderError) -> Self {
        Self::Store(error)
    }
}
