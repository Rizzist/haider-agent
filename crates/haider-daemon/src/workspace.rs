//! Cheap stored-workspace availability checks.
//!
//! Session creation and explicit re-rooting perform the expensive canonical
//! validation. Attach, hooks, and turn startup only prove that the already
//! canonical stored root still names an openable directory.

use std::path::Path;

use haider_protocol::workspace::{WorkspaceUnavailable, WorkspaceUnavailableReason};

#[must_use]
pub(crate) fn unavailable(path: &Path) -> Option<WorkspaceUnavailable> {
    let stored = path.display().to_string();
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let reason = if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceUnavailableReason::Missing
            } else {
                WorkspaceUnavailableReason::NotReadable
            };
            return Some(WorkspaceUnavailable {
                path: stored,
                reason,
                detail: bounded_detail(&error.to_string()),
            });
        }
    };
    if !metadata.is_dir() {
        return Some(WorkspaceUnavailable {
            path: stored,
            reason: WorkspaceUnavailableReason::NotDirectory,
            detail: "stored workspace root is not a directory".to_owned(),
        });
    }
    if let Err(error) = haider_platform::open_workspace_directory(path) {
        return Some(WorkspaceUnavailable {
            path: stored,
            reason: WorkspaceUnavailableReason::NotReadable,
            detail: bounded_detail(&error.to_string()),
        });
    }
    None
}

fn bounded_detail(detail: &str) -> String {
    const LIMIT: usize = 240;
    let sanitized: String = detail
        .chars()
        .filter(|character| !character.is_control())
        .take(LIMIT)
        .collect();
    if sanitized.is_empty() {
        "workspace root is unavailable".to_owned()
    } else {
        sanitized
    }
}

pub(crate) fn error(unavailable: &WorkspaceUnavailable) -> haider_protocol::error::HaiderError {
    haider_protocol::error::HaiderError::new(
        haider_protocol::error::ErrorCode::WorkspaceUnavailable,
        format!(
            "workspace unavailable: {}: {} ({})",
            unavailable.path,
            unavailable.reason.as_str(),
            unavailable.detail
        ),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_and_non_directory_roots() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        assert_eq!(
            unavailable(temporary.path()),
            None,
            "valid root stays usable"
        );
        let missing = temporary.path().join("missing");
        assert!(matches!(
            unavailable(&missing),
            Some(WorkspaceUnavailable {
                reason: WorkspaceUnavailableReason::Missing,
                ..
            })
        ));
        let file = temporary.path().join("file");
        std::fs::write(&file, b"not a directory")?;
        assert!(matches!(
            unavailable(&file),
            Some(WorkspaceUnavailable {
                reason: WorkspaceUnavailableReason::NotDirectory,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn typed_error_is_not_a_provider_error() {
        let unavailable = WorkspaceUnavailable {
            path: "/gone".into(),
            reason: WorkspaceUnavailableReason::Missing,
            detail: "not found".into(),
        };
        let error = error(&unavailable);
        assert_eq!(
            error.code,
            haider_protocol::error::ErrorCode::WorkspaceUnavailable
        );
        assert!(!error.retryable);
    }
}
