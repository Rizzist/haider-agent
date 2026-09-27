//! Regression checks for the idempotent lockdown publication fast path.
#![allow(clippy::expect_used)]

use crate::lockdown::{
    LockdownError, LockdownManager, PublicationIoCounts, with_publication_io_hook,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(unix)]
#[test]
fn activating_the_same_frozen_run_twice_keeps_the_ledger_inode() {
    use std::os::unix::fs::MetadataExt as _;

    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    manager
        .bind_and_activate_turn("profile", "session", "run", "provider", true, false)
        .expect("first publication");
    let ledger = root.join("turns.json");
    let before = std::fs::metadata(&ledger).expect("ledger metadata").ino();
    manager
        .bind_and_activate_turn("profile", "session", "run", "provider", false, false)
        .expect("idempotent publication keeps frozen policy");
    let after = std::fs::metadata(&ledger).expect("ledger metadata").ino();
    assert_eq!(
        before, after,
        "idempotent activation must not replace the ledger"
    );
    assert_eq!(
        manager
            .turn_binding("profile", "session", "run")
            .expect("binding lookup"),
        Some(("provider".to_owned(), true, false))
    );
}

/// MUTATION CHECK: removing the same-run shortcut produces a write and two
/// Full sync calls; applying it to a new run suppresses a required write.
#[test]
fn same_run_has_zero_publication_io_but_a_new_run_is_fully_synced() {
    for (lockdown, auto_hermetic) in [(false, false), (true, false), (true, true)] {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager =
            LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
        let counts = Arc::new(PublicationIoCounts::default());
        let full_syncs_per_publication = if cfg!(unix) { 2 } else { 1 };
        with_publication_io_hook(Arc::clone(&counts), || {
            manager
                .bind_and_activate_turn(
                    "profile",
                    "session",
                    "first",
                    "provider",
                    lockdown,
                    auto_hermetic,
                )
                .expect("first durable publication");
        });
        assert_eq!(counts.writes.load(Ordering::Relaxed), 1);
        assert_eq!(
            counts.syncs.load(Ordering::Relaxed),
            full_syncs_per_publication
        );

        with_publication_io_hook(Arc::clone(&counts), || {
            manager
                .bind_and_activate_turn("profile", "session", "first", "provider", false, false)
                .expect("frozen same-run activation");
        });
        assert_eq!(counts.writes.load(Ordering::Relaxed), 1);
        assert_eq!(
            counts.syncs.load(Ordering::Relaxed),
            full_syncs_per_publication
        );

        with_publication_io_hook(Arc::clone(&counts), || {
            manager
                .bind_and_activate_turn(
                    "profile",
                    "session",
                    "second",
                    "provider",
                    lockdown,
                    auto_hermetic,
                )
                .expect("new run must publish");
        });
        assert_eq!(counts.writes.load(Ordering::Relaxed), 2);
        assert_eq!(
            counts.syncs.load(Ordering::Relaxed),
            2 * full_syncs_per_publication
        );
        assert_eq!(
            manager
                .active_session_binding("profile", "session")
                .expect("active binding"),
            Some((
                "second".to_owned(),
                "provider".to_owned(),
                lockdown,
                auto_hermetic
            ))
        );

        // A previously frozen run needs a new active-boundary publication when
        // another run became active in the meantime.
        with_publication_io_hook(Arc::clone(&counts), || {
            manager
                .bind_and_activate_turn("profile", "session", "first", "provider", false, false)
                .expect("switch active boundary back to first run");
        });
        assert_eq!(counts.writes.load(Ordering::Relaxed), 3);
        assert_eq!(
            counts.syncs.load(Ordering::Relaxed),
            3 * full_syncs_per_publication
        );

        let restarted =
            LockdownManager::initialize(fixture.path().join("lockdown")).expect("restart manager");
        with_publication_io_hook(Arc::clone(&counts), || {
            restarted
                .bind_and_activate_turn("profile", "session", "first", "provider", false, false)
                .expect("same frozen run after restart");
        });
        assert_eq!(counts.writes.load(Ordering::Relaxed), 3);
        assert_eq!(
            counts.syncs.load(Ordering::Relaxed),
            3 * full_syncs_per_publication
        );
        assert_eq!(
            counts.full_syncs.load(Ordering::Relaxed),
            3 * full_syncs_per_publication
        );
    }
}

/// The separate activation used by explicit Full rebind recovery obeys the
/// same rule as combined binding: an identical active boundary needs no I/O.
#[test]
fn activating_an_already_active_frozen_run_has_zero_publication_io() {
    for (lockdown, auto_hermetic) in [(false, false), (true, false), (true, true)] {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("lockdown");
        let manager = LockdownManager::initialize(root.clone()).expect("manager");
        let counts = Arc::new(PublicationIoCounts::default());
        let syncs_per_publication = if cfg!(unix) { 2 } else { 1 };
        let assert_counts = |writes| {
            assert_eq!(counts.writes.load(Ordering::Relaxed), writes);
            assert_eq!(
                counts.syncs.load(Ordering::Relaxed),
                writes * syncs_per_publication
            );
            assert_eq!(
                counts.full_syncs.load(Ordering::Relaxed),
                writes * syncs_per_publication
            );
        };
        let activate = |manager: &LockdownManager, run: &str| {
            with_publication_io_hook(Arc::clone(&counts), || {
                manager
                    .activate_turn("profile", "session", run)
                    .expect("activate durable binding");
            });
        };
        with_publication_io_hook(Arc::clone(&counts), || {
            assert!(matches!(
                manager.activate_turn("profile", "session", "missing"),
                Err(LockdownError::InvalidLedger { .. })
            ));
        });
        assert_counts(0);
        manager
            .bind_turn(
                "profile",
                "session",
                "first",
                "provider",
                lockdown,
                auto_hermetic,
            )
            .expect("bind first run");
        activate(&manager, "first");
        assert_counts(1);
        activate(&manager, "first");
        assert_counts(1);
        manager
            .bind_turn(
                "profile",
                "session",
                "second",
                "provider",
                lockdown,
                auto_hermetic,
            )
            .expect("bind another run");
        activate(&manager, "second");
        assert_counts(2);
        activate(&manager, "first");
        assert_counts(3);

        let restarted = LockdownManager::initialize(root).expect("restart manager");
        activate(&restarted, "first");
        assert_counts(3);
        if lockdown && !auto_hermetic {
            restarted
                .bind_turn("profile", "session", "first", "provider", true, true)
                .expect("narrow durable record before activation");
            activate(&restarted, "first");
            assert_counts(4);
            activate(&restarted, "first");
            assert_counts(4);
            assert_eq!(
                restarted
                    .active_session_binding("profile", "session")
                    .expect("narrowed active binding"),
                Some(("first".to_owned(), "provider".to_owned(), true, true))
            );
        }
    }
}

#[tokio::test]
async fn panicked_blocking_task_returns_typed_lockdown_error() {
    let error = crate::lockdown::on_blocking_pool(|| -> Result<(), LockdownError> {
        panic!("injected blocking task failure")
    })
    .await
    .expect_err("join failure must fail closed");
    assert!(matches!(error, LockdownError::Task { .. }), "{error:?}");
    let nested = crate::lockdown::on_blocking_pool_result(|| -> Result<(), LockdownError> {
        panic!("injected domain-result task failure")
    })
    .await
    .expect_err("domain-result join failure must fail closed");
    assert!(matches!(nested, LockdownError::Task { .. }), "{nested:?}");
}

#[test]
fn auto_hermetic_narrowing_of_the_same_run_still_publishes_with_full_sync() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager
        .bind_and_activate_turn("profile", "session", "run", "provider", true, false)
        .expect("configured lockdown");
    let counts = Arc::new(PublicationIoCounts::default());
    with_publication_io_hook(Arc::clone(&counts), || {
        manager
            .bind_and_activate_turn("profile", "session", "run", "provider", true, true)
            .expect("exact auto-hermetic narrowing");
    });
    assert_eq!(counts.writes.load(Ordering::Relaxed), 1);
    assert_eq!(
        counts.syncs.load(Ordering::Relaxed),
        if cfg!(unix) { 2 } else { 1 }
    );
    assert_eq!(
        counts.full_syncs.load(Ordering::Relaxed),
        if cfg!(unix) { 2 } else { 1 }
    );
    assert_eq!(
        manager
            .turn_binding("profile", "session", "run")
            .expect("binding"),
        Some(("provider".to_owned(), true, true))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ledger_operation_runs_outside_the_async_worker() {
    let async_thread = std::thread::current().id();
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    crate::lockdown::on_blocking_pool(move || {
        assert_ne!(std::thread::current().id(), async_thread);
        let manager = LockdownManager::initialize(root)?;
        manager.bind_and_activate_turn("profile", "session", "run", "provider", true, false)?;
        Ok(())
    })
    .await
    .expect("ledger operation on blocking pool");
}

/// MUTATION CHECK: publishing the local binding before the durable call makes
/// the first injected window report a binding that restart cannot enforce.
#[test]
fn crash_between_durable_publish_and_execute_recovers_the_frozen_binding() {
    const TEST_NAME: &str = "lockdown_binding_tests::crash_between_durable_publish_and_execute_recovers_the_frozen_binding";
    if let Some(stage) = std::env::var_os("HAIDER_LOCKDOWN_CRASH_STAGE") {
        let root = std::path::PathBuf::from(
            std::env::var_os("HAIDER_LOCKDOWN_CRASH_ROOT").expect("child root"),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let store = haider_core::SqliteStoreHandle::open(root.join("profile"))
                .await
                .expect("store");
            crate::lockdown::initialize_global(Some(&root.join("lockdown")))
                .expect("lockdown manager");
            let hub =
                crate::SessionHub::new(store, crate::SessionHubConfig::default()).expect("hub");
            let session = haider_protocol::ids::SessionId::new("crash-session");
            let run = haider_protocol::ids::RunId::new("crash-run");
            hub.bind_activate_lockdown_turn(
                &session,
                &run,
                "provider",
                crate::auto_hermetic::ProviderLockdownPolicy::Configured,
            )
            .expect("bind and publish");
        });
        panic!("crash checkpoint {stage:?} failed to abort");
    }

    for stage in [
        "after_durable_before_publish",
        "after_publish_before_execute",
    ] {
        let fixture = tempfile::tempdir().expect("fixture");
        let marker = fixture.path().join("checkpoint");
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env("HAIDER_LOCKDOWN_CRASH_STAGE", stage)
            .env("HAIDER_LOCKDOWN_ASSERT_DURABLE_BEFORE_PUBLISH", "1")
            .env("HAIDER_LOCKDOWN_CRASH_ROOT", fixture.path())
            .env("HAIDER_LOCKDOWN_CRASH_MARKER", &marker)
            .output()
            .expect("launch crash child");
        assert!(!child.status.success(), "child must abort at {stage}");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap_or_else(|error| {
                panic!(
                    "missing checkpoint at {stage}: {error}; child stdout: {}; child stderr: {}",
                    String::from_utf8_lossy(&child.stdout),
                    String::from_utf8_lossy(&child.stderr)
                )
            }),
            stage
        );
        let root = fixture.path().join("lockdown");
        let bytes = std::fs::read(root.join("turns.json")).expect("durable ledger");
        let ledger: serde_json::Value = serde_json::from_slice(&bytes).expect("ledger JSON");
        let profile = ledger["bindings"][0]["profile_id"]
            .as_str()
            .expect("durable profile id");
        let restarted = LockdownManager::initialize(root).expect("restart manager");
        assert_eq!(
            restarted
                .bind_turn(
                    profile,
                    "crash-session",
                    "crash-run",
                    "provider",
                    false,
                    false
                )
                .expect("frozen policy wins after restart"),
            ("provider".to_owned(), true, false)
        );
        assert!(matches!(
            restarted.bind_turn(
                profile,
                "crash-session",
                "crash-run",
                "forged",
                false,
                false
            ),
            Err(LockdownError::TurnBindingConflict { .. })
        ));
        assert_eq!(
            restarted
                .active_session_binding(profile, "crash-session")
                .expect("active after restart"),
            Some(("crash-run".to_owned(), "provider".to_owned(), true, false))
        );
    }
}

/// The blocking pool may schedule two writers together. The flock must keep
/// each read-modify-write transaction intact, including the global quota.
#[test]
fn concurrent_bindings_and_quota_writes_keep_both_global_ledgers_serialized() {
    use std::path::Path;
    use std::sync::Barrier;

    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    manager.set_quota(1).expect("one byte quota");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for index in 0..2 {
        let manager = manager.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            manager
                .bind_and_activate_turn(
                    "profile",
                    "session",
                    &format!("run-{index}"),
                    "provider",
                    true,
                    false,
                )
                .expect("serialized binding");
            manager.write(&format!("provider-{index}"), Path::new("one"), b"x")
        }));
    }
    barrier.wait();
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().expect("writer thread"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(LockdownError::LockdownQuotaExceeded { .. })))
            .count(),
        1
    );
    assert_eq!(manager.status(None).expect("quota status").quota_used, 1);
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("turns.json")).expect("turn ledger"))
            .expect("ledger JSON");
    let bindings = ledger["bindings"].as_array().expect("binding array");
    assert_eq!(bindings.len(), 2, "neither concurrent bind may be lost");
    assert_eq!(ledger["active"].as_array().expect("active array").len(), 1);
}

/// Independent processes share the machine-global flock, not a Rust mutex.
#[test]
fn concurrent_profile_processes_share_bindings_and_global_quota() {
    const TEST_NAME: &str =
        "lockdown_binding_tests::concurrent_profile_processes_share_bindings_and_global_quota";
    if let Some(root) = std::env::var_os("HAIDER_LOCKDOWN_PROCESS_ROOT") {
        let root = std::path::PathBuf::from(root);
        let profile = std::env::var("HAIDER_LOCKDOWN_PROCESS_PROFILE").expect("child profile");
        let manager = LockdownManager::initialize(root.clone()).expect("child manager");
        let coordination = root.parent().expect("coordination directory");
        std::fs::write(coordination.join(format!("{profile}.ready")), b"ready")
            .expect("child ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !coordination.join("go").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent releases child gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        manager
            .bind_and_activate_turn(&profile, "session", "run", "provider", true, false)
            .expect("child publication");
        let result = match manager.write(&profile, std::path::Path::new("one"), b"x") {
            Ok(_) => "written",
            Err(LockdownError::LockdownQuotaExceeded { .. }) => "quota_refused",
            Err(error) => panic!("unexpected write error: {error}"),
        };
        std::fs::write(coordination.join(format!("{profile}.result")), result)
            .expect("child result");
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    let coordination = fixture.path();
    manager.set_quota(1).expect("global one-byte quota");
    let mut children = Vec::new();
    for profile in ["profile-a", "profile-b"] {
        children.push(
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env("HAIDER_LOCKDOWN_PROCESS_ROOT", &root)
                .env("HAIDER_LOCKDOWN_PROCESS_PROFILE", profile)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("launch profile child"),
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !coordination.join("profile-a.ready").exists()
        || !coordination.join("profile-b.ready").exists()
    {
        assert!(std::time::Instant::now() < deadline, "children initialize");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write(coordination.join("go"), b"go").expect("release concurrent children");
    for child in children {
        let output = child.wait_with_output().expect("wait for profile child");
        assert!(
            output.status.success(),
            "child stdout: {}; child stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut outcomes = ["profile-a", "profile-b"].map(|profile| {
        std::fs::read_to_string(coordination.join(format!("{profile}.result")))
            .expect("quota outcome")
    });
    outcomes.sort();
    assert_eq!(outcomes, ["quota_refused", "written"]);
    for profile in ["profile-a", "profile-b"] {
        assert_eq!(
            manager
                .turn_binding(profile, "session", "run")
                .expect("durable binding"),
            Some(("provider".to_owned(), true, false))
        );
    }
    assert_eq!(
        manager
            .status(None)
            .expect("global quota status")
            .quota_used,
        1
    );
}
