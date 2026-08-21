//! Daemon-facing adapter for the shared platform endpoint owner.

use crate::{DaemonConfig, DaemonError};

pub(crate) type BoundEndpoint = haider_platform::BoundEndpoint;

pub(crate) async fn bind(config: &DaemonConfig) -> Result<BoundEndpoint, DaemonError> {
    let endpoint = haider_platform::Endpoint::new(&config.runtime_dir, &config.profile_id);
    let bound = BoundEndpoint::bind(&endpoint, &config.runtime_dir)
        .await
        .map_err(map_error)?;
    // Hygiene, never correctness: a daemon killed without running its cleanup
    // (SIGKILL, panic, power loss) leaves its endpoint node behind, and those
    // accumulate forever in a shared runtime directory — 1259 of them were
    // observed on one development machine, where a stale node is
    // indistinguishable from a live one by name or metadata and only a
    // connect tells them apart. Sweep the dead ones on the way up, bounded,
    // proving death exactly as the bind path does. Failures are ignored.
    let removed =
        haider_platform::sweep_stale_endpoints(&config.runtime_dir, Some(bound.path())).await;
    if removed > 0 {
        tracing::info!(
            removed,
            "swept stale endpoint nodes from the runtime directory"
        );
    }
    Ok(bound)
}

pub(crate) fn map_error(error: haider_platform::EndpointError) -> DaemonError {
    match error {
        haider_platform::EndpointError::Io {
            operation,
            path,
            source,
        } => DaemonError::Io {
            operation,
            path,
            source,
        },
        haider_platform::EndpointError::Endpoint { message } => DaemonError::Endpoint { message },
        haider_platform::EndpointError::Task { message } => DaemonError::Task { message },
    }
}

impl From<haider_platform::EndpointError> for DaemonError {
    fn from(error: haider_platform::EndpointError) -> Self {
        map_error(error)
    }
}
