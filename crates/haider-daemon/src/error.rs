use haider_protocol::error::HaiderError;
use std::path::PathBuf;

/// Best-effort facts about the incumbent lock holder.
///
/// These values are diagnostic only; the OS-held profile lock remains the
/// sole singleton authority (R1) — PID/socket contents are never trusted to
/// decide liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncumbentDiagnostics {
    pub profile_id: String,
    /// Where the incumbent's endpoint would live; not probed, not verified.
    pub endpoint_path: PathBuf,
    /// Truncated contents of the store's owner-diagnostics file, if readable.
    pub lock_contents: Option<String>,
}

/// Typed failure surface of the daemon lifecycle.
#[derive(Debug)]
pub enum DaemonError {
    /// Another process holds the profile's lifetime lock. This is the loser's
    /// clean exit in the R1/R4 singleton race, not a fault.
    AlreadyRunning { diagnostics: IncumbentDiagnostics },
    /// [`crate::DaemonConfig`] failed validation before any resource was touched.
    InvalidConfig { message: String },
    /// The profile store refused an operation (open, generation bump, append,
    /// flush, close); carries the store's own typed error.
    Store(HaiderError),
    /// A named filesystem/socket syscall failed at a named path.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The longest endpoint address cannot fit the platform IPC namespace.
    EndpointAddressTooLong {
        path: PathBuf,
        length: usize,
        limit: usize,
        unit: &'static str,
    },
    /// The rendezvous endpoint could not be owned safely (unverified stale
    /// socket, wrong directory owner, live socket under a held lock, ...).
    Endpoint { message: String },
    /// A connection violated the wire contract or its bounded queue.
    Protocol { message: String },
    /// A daemon-owned task or process-level facility failed (spawn, join,
    /// signal handler, randomness).
    Task { message: String },
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

    /// Process exit code for `haiderd`, following BSD `sysexits.h`:
    /// 75 `EX_TEMPFAIL` (already running — retry/attach is reasonable),
    /// 64 `EX_USAGE`, 74 `EX_IOERR`, 70 `EX_SOFTWARE`.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::AlreadyRunning { .. } => 75,
            Self::InvalidConfig { .. } => 64,
            Self::Store(_)
            | Self::Io { .. }
            | Self::EndpointAddressTooLong { .. }
            | Self::Endpoint { .. } => 74,
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
            Self::Store(error) => write!(
                formatter,
                "daemon store error ({:?}): {}",
                error.code, error.message
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::EndpointAddressTooLong {
                path,
                length,
                limit,
                unit,
            } => write!(
                formatter,
                "daemon endpoint path {} is {length} {unit}; platform IPC limit is {limit} {unit}",
                path.display()
            ),
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
