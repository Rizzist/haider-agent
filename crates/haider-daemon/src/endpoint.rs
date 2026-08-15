//! Daemon-facing adapter for the shared platform endpoint owner.

use crate::{DaemonConfig, DaemonError};

pub(crate) type BoundEndpoint = haider_platform::BoundEndpoint;

pub(crate) async fn bind(config: &DaemonConfig) -> Result<BoundEndpoint, DaemonError> {
    let endpoint = haider_platform::Endpoint::new(&config.runtime_dir, &config.profile_id);
    BoundEndpoint::bind(&endpoint, &config.runtime_dir)
        .await
        .map_err(map_error)
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
