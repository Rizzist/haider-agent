#![allow(clippy::unwrap_used)]
use super::contract::*;

#[test]
fn policy_v1_is_exact_and_never_accepts_caller_allowlists() {
    let valid = serde_json::json!({"policy_version":1,"name":"android-standalone","default_model":"openai/gpt-test","store_synchronous":"normal"});
    assert!(Policy::parse(&valid.to_string()).is_ok());
    for (key, value) in [
        ("policy_version", serde_json::json!(2)),
        ("name", serde_json::json!("desktop")),
        ("default_model", serde_json::json!(" \n")),
        ("store_synchronous", serde_json::json!("off")),
        ("allowlist", serde_json::json!(["exec"])),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert_eq!(
            Policy::parse(&invalid.to_string()).unwrap_err(),
            NativeStatus::BadPolicy
        );
    }
    let mut full = valid;
    full["store_synchronous"] = serde_json::json!("full");
    assert_eq!(
        Policy::parse(&full.to_string()).unwrap().store_synchronous,
        haider_protocol::runtime::StoreSynchronous::Full
    );
}

#[test]
fn observation_omits_unknown_facts_and_uses_the_single_status_domain() {
    let stopped = serde_json::to_value(Observation::phase("Stopped", 0)).unwrap();
    assert_eq!(
        stopped,
        serde_json::json!({"jni_version":1,"phase":"Stopped","daemon_generation":0})
    );
    let failed =
        serde_json::to_value(Observation::failed(NativeStatus::StoreRecoveryFailed, 17)).unwrap();
    assert_eq!(failed["error_code"], "STORE_RECOVERY_FAILED");
    assert_eq!(failed["daemon_generation"], 17);
    assert!(failed.get("endpoint_path").is_none());
    assert_eq!(NativeStatus::ShutdownForced as i32, 9);
    assert_eq!(shutdown_budget(-1), Err(NativeStatus::BadArgument));
    assert_eq!(shutdown_budget(0).unwrap().as_millis(), 7000);
    assert_eq!(shutdown_budget(2).unwrap().as_millis(), 2);
}

#[cfg(unix)]
#[test]
fn paths_require_exact_private_layout_and_both_socket_budgets() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    // Keep the real path short enough for macOS's stricter staging bound.
    let root = tempfile::Builder::new()
        .prefix("a")
        .tempdir_in("/tmp")
        .unwrap();
    let root = root.path().canonicalize().unwrap();
    let paths = Paths {
        profile_id: "android-default".into(),
        store_dir: root.join("haider/profiles/default"),
        runtime_dir: root.join("haider/runtime/android-default"),
        logs_dir: root.join("haider/logs"),
        workspace_dir: root.join("haider/profiles/default/workspace"),
        tmp_dir: root.join("haider/runtime/android-default/tmp"),
    };
    for path in [
        &paths.store_dir,
        &paths.runtime_dir,
        &paths.logs_dir,
        &paths.workspace_dir,
        &paths.tmp_dir,
    ] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    paths.validate(&root).unwrap();
    let mut bad = paths.clone();
    bad.profile_id = "other".into();
    assert_eq!(bad.validate(&root), Err(NativeStatus::BadPaths));
    let mut bad = paths.clone();
    bad.tmp_dir = std::path::PathBuf::from("/tmp");
    assert!(bad.validate(&root).is_err());
    std::fs::remove_dir(&paths.workspace_dir).unwrap();
    symlink(&paths.logs_dir, &paths.workspace_dir).unwrap();
    assert!(paths.validate(&root).is_err());
}

#[cfg(unix)]
#[test]
fn exact_c3_layout_rejects_first_over_budget_staging_path() {
    use std::os::unix::fs::PermissionsExt;
    // Endpoint staging uses a 20-byte basename and a slash. Construct otherwise
    // valid C3 layouts at the platform limit and one byte beyond it; the final
    // h.sock/mobile.sock addresses themselves fit in both cases.
    let budget = if cfg!(any(target_os = "linux", target_os = "android")) {
        107
    } else {
        103
    };
    let base = tempfile::Builder::new()
        .prefix("b")
        .tempdir_in("/tmp")
        .unwrap();
    let base_path = base.path().canonicalize().unwrap();
    let suffix = "/haider/runtime/android-default";
    for extra in [0, 1] {
        let root_length = budget - 21 - suffix.len() + extra;
        let root = base_path.join("x".repeat(root_length - base_path.as_os_str().len() - 1));
        let paths = Paths {
            profile_id: "android-default".into(),
            store_dir: root.join("haider/profiles/default"),
            runtime_dir: root.join("haider/runtime/android-default"),
            logs_dir: root.join("haider/logs"),
            workspace_dir: root.join("haider/profiles/default/workspace"),
            tmp_dir: root.join("haider/runtime/android-default/tmp"),
        };
        for path in [
            &paths.store_dir,
            &paths.runtime_dir,
            &paths.logs_dir,
            &paths.workspace_dir,
            &paths.tmp_dir,
        ] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(paths.runtime_dir.as_os_str().len() + 21, budget + extra);
        assert!(paths.runtime_dir.join("mobile.sock").as_os_str().len() < budget);
        if extra == 0 {
            paths.validate(&root).unwrap();
        } else {
            assert_eq!(paths.validate(&root), Err(NativeStatus::BadPaths));
        }
    }
}
