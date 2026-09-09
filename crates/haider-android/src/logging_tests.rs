#![allow(clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn native_logs_redact_paths_rotate_and_reject_unsafe_files() {
    let root = tempfile::tempdir().unwrap();
    let mut observation = Observation::phase("Ready", 7);
    observation.endpoint_path = Some("/synthetic-private-directory/h.sock".into());
    record(root.path(), &observation).unwrap();
    let current = root.path().join("native.log");
    let bytes = std::fs::read(&current).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-private-directory"));
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["daemon_generation"], 7);
    assert_eq!(
        std::fs::metadata(&current).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for _ in 0..6 {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&current)
            .unwrap()
            .set_len(1_048_576)
            .unwrap();
        record(root.path(), &observation).unwrap();
    }
    assert!(root.path().join("native.log.4").is_file());
    assert!(!root.path().join("native.log.5").exists());
    assert!(std::fs::metadata(&current).unwrap().len() < 1024);

    let outside = root.path().join("sentinel");
    std::fs::write(&outside, b"untouched").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&current).unwrap();
    symlink(&outside, &current).unwrap();
    assert!(record(root.path(), &observation).is_err());
    std::fs::remove_file(&current).unwrap();
    std::fs::hard_link(&outside, &current).unwrap();
    assert!(record(root.path(), &observation).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
    std::fs::remove_file(&current).unwrap();
    std::fs::write(&current, b"not private").unwrap();
    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(record(root.path(), &observation).is_err());
}
