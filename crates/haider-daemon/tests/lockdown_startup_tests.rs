#![allow(clippy::expect_used)]

use std::fs;
use std::time::Duration;

use haider_core::SqliteStoreHandle;
use haider_daemon::{DaemonConfig, DaemonError, DaemonState, spawn};

#[tokio::test]
async fn lockdown_init_failure_never_readies_and_releases_profile_lease() {
    let fixture = tempfile::tempdir().expect("fixture");
    let invalid_root = fixture.path().join("not-a-directory");
    fs::write(&invalid_root, b"fixture").expect("create invalid lockdown root");

    let store = fixture.path().join("profile");
    let runtime = fixture.path().join("runtime");
    let mut failed_config = DaemonConfig::new("lockdown-init-test", &store, &runtime);
    failed_config.lockdown_root_override = Some(invalid_root);
    failed_config.discovery_disabled = true;
    let task = spawn(failed_config);
    let readiness = task.readiness();
    let failure = tokio::time::timeout(Duration::from_secs(60), task.join())
        .await
        .expect("initialization finishes")
        .expect_err("invalid lockdown root must fail startup");
    assert!(
        matches!(failure, DaemonError::Lockdown { .. }),
        "{failure:?}"
    );
    assert!(matches!(readiness.current(), DaemonState::Failed { .. }));
    assert!(!readiness.snapshot().ready);

    // The failed startup may have opened SQLite, but it must have released
    // the profile lease before returning the typed lockdown error.
    let lease = tokio::time::timeout(
        Duration::from_secs(60),
        SqliteStoreHandle::acquire_profile(&store),
    )
    .await
    .expect("lease acquisition finishes")
    .expect("profile lease is free");
    drop(lease);
}

#[tokio::test]
async fn malformed_lockdown_ledger_failure_releases_flock_and_profile_lease() {
    let fixture = tempfile::tempdir().expect("fixture");
    let lockdown_root = fixture.path().join("lockdown");
    fs::create_dir(&lockdown_root).expect("lockdown root");
    fs::write(lockdown_root.join("quota.json"), b"not-json").expect("malformed ledger");
    let store = fixture.path().join("profile");
    let mut config = DaemonConfig::new(
        "lockdown-ledger-test",
        &store,
        fixture.path().join("runtime"),
    );
    config.lockdown_root_override = Some(lockdown_root.clone());
    config.discovery_disabled = true;
    let task = spawn(config);
    let readiness = task.readiness();
    let failure = tokio::time::timeout(Duration::from_secs(60), task.join())
        .await
        .expect("initialization finishes")
        .expect_err("malformed quota ledger must fail startup");
    assert!(
        matches!(failure, DaemonError::Lockdown { .. }),
        "{failure:?}"
    );
    assert!(!readiness.snapshot().ready);

    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lockdown_root.join("quota.lock"))
        .expect("quota lock file");
    haider_platform::try_lock_file_exclusive(&lock_file).expect("quota flock released");
    haider_platform::unlock_file(&lock_file).expect("quota flock unlock");
    let lease = SqliteStoreHandle::acquire_profile(&store)
        .await
        .expect("profile lease released");
    drop(lease);
}

/// Hold the real global flock so store opening can finish while initialization
/// is still blocked. A delayed failure must never expose accounts or Ready.
#[tokio::test]
async fn lockdown_join_precedes_every_startup_publication_surface() {
    let fixture = tempfile::tempdir().expect("fixture");
    let lockdown_root = fixture.path().join("lockdown");
    fs::create_dir(&lockdown_root).expect("lockdown root");
    fs::write(lockdown_root.join("quota.json"), b"not-json").expect("malformed ledger");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lockdown_root.join("quota.lock"))
        .expect("quota lock file");
    haider_platform::lock_file_exclusive(&lock_file).expect("delay initialization");
    let store = fixture.path().join("profile");
    let mut config = DaemonConfig::new(
        "lockdown-order-test",
        &store,
        fixture.path().join("runtime"),
    );
    config.lockdown_root_override = Some(lockdown_root);
    config.discovery_disabled = true;
    let endpoint = config.endpoint_path();
    let task = spawn(config);
    let readiness = task.readiness();
    let diagnostics = task.diagnostics();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !store.join("store.sqlite").exists() {
            assert!(
                !diagnostics.snapshot().finished,
                "startup must wait for initialization"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("store opens concurrently with blocked lockdown initialization");
    // Observe a stable blocked window, rather than just the final Failed
    // state, which could hide an earlier erroneous Ready publication.
    for _ in 0..100 {
        assert!(matches!(readiness.current(), DaemonState::Recovering));
        assert!(!readiness.snapshot().ready);
        assert!(!readiness.snapshot().providers_loaded);
        assert!(
            diagnostics.snapshot().bootstrap.is_none(),
            "listener must follow initialization"
        );
        assert!(!endpoint.exists(), "endpoint must follow initialization");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    haider_platform::unlock_file(&lock_file).expect("release delayed initialization");
    let failure = tokio::time::timeout(Duration::from_secs(60), task.join())
        .await
        .expect("initialization finishes")
        .expect_err("delayed malformed ledger must fail startup");
    assert!(
        matches!(failure, DaemonError::Lockdown { .. }),
        "{failure:?}"
    );
    assert!(!readiness.snapshot().ready);
    let lease = SqliteStoreHandle::acquire_profile(&store)
        .await
        .expect("profile lease released");
    drop(lease);
    haider_platform::try_lock_file_exclusive(&lock_file).expect("initialization released flock");
    haider_platform::unlock_file(&lock_file).expect("flock unlock");
}

/// Every early resource failure must join the concurrently started task. The
/// typed initialization error wins, and resources can be reacquired afterward.
#[tokio::test]
async fn early_startup_failures_join_lockdown_initialization() {
    for failure_kind in ["profile_locked", "runtime_directory", "store_open"] {
        let fixture = tempfile::tempdir().expect("fixture");
        let invalid_root = fixture.path().join("not-a-directory");
        fs::write(&invalid_root, b"fixture").expect("invalid lockdown root");
        let store = fixture.path().join("profile");
        let runtime = fixture.path().join("runtime");
        let incumbent = if failure_kind == "profile_locked" {
            Some(
                SqliteStoreHandle::acquire_profile(&store)
                    .await
                    .expect("incumbent lease"),
            )
        } else {
            None
        };
        if failure_kind == "runtime_directory" {
            fs::write(&runtime, b"fixture").expect("invalid runtime directory");
        }
        if failure_kind == "store_open" {
            fs::create_dir_all(store.join("store.sqlite")).expect("invalid store database");
        }
        let mut config = DaemonConfig::new("lockdown-early-test", &store, &runtime);
        config.lockdown_root_override = Some(invalid_root);
        config.discovery_disabled = true;
        let task = spawn(config);
        let readiness = task.readiness();
        let failure = tokio::time::timeout(Duration::from_secs(60), task.join())
            .await
            .expect("early failure finishes")
            .expect_err("initialization must fail");
        assert!(
            matches!(failure, DaemonError::Lockdown { .. }),
            "{failure_kind}: {failure:?}"
        );
        assert!(!readiness.snapshot().ready);
        drop(incumbent);
        let lease = SqliteStoreHandle::acquire_profile(&store)
            .await
            .expect("profile lease released");
        drop(lease);
    }
}
