#![allow(clippy::expect_used, clippy::unwrap_used)]
use crate::lockdown::{
    LockdownError, LockdownManager, PublicationIoCounts, with_publication_io_fault,
    with_publication_io_hook,
};
use crate::session_hub::SessionHubError;
use crate::session_hub::lockdown_save::SaveOwners;
use haider_protocol::ids::{RunId, SessionId};
use std::sync::{Arc, atomic::Ordering};

#[test]
fn cancelled_and_superseded_saves_cannot_activate_either_path() {
    for standalone in [false, true] {
        for variant in [
            "cancel",
            "timeout",
            "restart",
            "two-successors",
            "aba",
            "delete",
            "same-run-new-generation",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().to_str().unwrap();
            let manager = LockdownManager::initialize(temp.path().join("lockdown")).unwrap();
            let owners = SaveOwners::default();
            let replacement = SaveOwners::default();
            let session = SessionId::new("session");
            let run = RunId::new("old");
            manager
                .bind_turn(profile, "session", "old", "provider", false, false)
                .unwrap();
            let old = owners.begin(profile, &session, &run).unwrap();
            let stale = old.permit();
            if matches!(variant, "cancel" | "timeout") {
                drop(old);
            } else {
                owners.fence(Some(&session), Some(&run));
            }
            let active_owners = if variant == "restart" {
                &replacement
            } else {
                &owners
            };
            let next = active_owners
                .begin(profile, &session, &RunId::new("new"))
                .unwrap();
            next.permit()
                .commit(|| {
                    manager
                        .bind_and_activate_turn(profile, "session", "new", "provider", true, false)
                        .unwrap();
                    Ok(())
                })
                .unwrap();
            let final_run = if variant == "two-successors" {
                "newer"
            } else if matches!(variant, "aba" | "same-run-new-generation") {
                "old"
            } else {
                "new"
            };
            let latest = active_owners
                .begin(profile, &session, &RunId::new(final_run))
                .unwrap();
            latest
                .permit()
                .commit(|| {
                    manager
                        .bind_and_activate_turn(
                            profile, "session", final_run, "provider", true, false,
                        )
                        .unwrap();
                    Ok(())
                })
                .unwrap();
            if variant == "delete" {
                active_owners.fence(Some(&session), None);
                manager.remove_session_bindings(profile, "session").unwrap();
            }
            let before = std::fs::read(temp.path().join("lockdown/turns.json")).unwrap();
            let result = stale.commit(|| {
                if standalone {
                    manager.activate_turn(profile, "session", "old").unwrap();
                } else {
                    manager
                        .bind_and_activate_turn(profile, "session", "old", "provider", false, false)
                        .unwrap();
                }
                Ok(())
            });
            assert!(
                matches!(result, Err(SessionHubError::SettingsSaveSuperseded { .. })),
                "{variant}/{standalone}: {result:?}"
            );
            assert_eq!(
                std::fs::read(temp.path().join("lockdown/turns.json")).unwrap(),
                before
            );
            // Old hub cleanup must not revoke the successor's save after restart.
            if variant == "restart" {
                owners.fence(None, None);
                latest.permit().ensure_current().unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_awaiting_future_revokes_its_detached_blocking_save() {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().to_str().unwrap().to_owned();
    let owners = Arc::new(SaveOwners::default());
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let (done, result) = tokio::sync::oneshot::channel();
    let owner = owners.clone();
    let task = tokio::spawn(async move {
        let guard = owner
            .begin(&profile, &SessionId::new("s"), &RunId::new("r"))
            .unwrap();
        let save = guard.permit();
        crate::lockdown::on_blocking_pool_result(move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            let result: Result<(), SessionHubError> =
                save.commit(|| panic!("cancelled save must never run"));
            done.send(result).unwrap();
            Ok::<(), SessionHubError>(())
        })
        .await
        .unwrap()
        .unwrap();
    });
    started.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    assert!(matches!(
        result.await.unwrap(),
        Err(SessionHubError::SettingsSaveSuperseded { .. })
    ));
}

#[test]
fn ownership_is_rechecked_under_the_file_lock_and_before_replacement() {
    for standalone in [false, true] {
        for expire_at in [1, 2] {
            let temp = tempfile::tempdir().unwrap();
            let manager = LockdownManager::initialize(temp.path().join("lockdown")).unwrap();
            manager
                .bind_and_activate_turn("p", "s", "old", "provider", true, false)
                .unwrap();
            manager
                .bind_turn("p", "s", "new", "provider", false, false)
                .unwrap();
            let before = std::fs::read(temp.path().join("lockdown/turns.json")).unwrap();
            let calls = std::cell::Cell::new(0);
            let current = || {
                let call = calls.get();
                calls.set(call + 1);
                call < expire_at
            };
            let result = if standalone {
                manager.activate_turn_if_current("p", "s", "new", current)
            } else {
                manager
                    .bind_and_activate_turn_if_current(
                        "p", "s", "new", "provider", false, false, current,
                    )
                    .map(|_| ())
            };
            assert!(matches!(result, Err(LockdownError::SaveSuperseded)));
            assert_eq!(
                std::fs::read(temp.path().join("lockdown/turns.json")).unwrap(),
                before
            );
            assert!(!temp.path().join("lockdown/turns.tmp").exists());
        }
    }
}

#[test]
fn storage_failure_retries_require_full_confirmation_in_both_apis() {
    for standalone in [false, true] {
        for case in [
            "full",
            "configured",
            "auto",
            "switch",
            "configured-narrow",
            "full-narrow",
        ] {
            for reopen in [false, true] {
                for stage in [1, 2, 3, 4] {
                    let temp = tempfile::tempdir().unwrap();
                    let root = temp.path().join("lockdown");
                    let manager = LockdownManager::initialize(root.clone()).unwrap();
                    let auto = matches!(case, "auto" | "configured-narrow" | "full-narrow");
                    let restricted = case != "full";
                    if matches!(case, "switch" | "configured-narrow" | "full-narrow") {
                        manager
                            .bind_and_activate_turn(
                                "p",
                                "s",
                                "r",
                                "provider",
                                case != "full-narrow",
                                false,
                            )
                            .unwrap();
                        if case == "switch" {
                            manager
                                .bind_and_activate_turn("p", "s", "other", "provider", true, false)
                                .unwrap();
                        }
                    }
                    if standalone {
                        manager
                            .bind_turn("p", "s", "r", "provider", restricted, auto)
                            .unwrap();
                    }
                    let apply = |manager: &LockdownManager| {
                        if standalone {
                            manager.activate_turn("p", "s", "r")
                        } else {
                            manager
                                .bind_and_activate_turn("p", "s", "r", "provider", restricted, auto)
                                .map(|_| ())
                        }
                    };
                    assert!(with_publication_io_fault(stage, || apply(&manager)).is_err());
                    let manager = if reopen {
                        LockdownManager::initialize(root).unwrap()
                    } else {
                        manager
                    };
                    assert!(
                        with_publication_io_fault(stage, || apply(&manager)).is_err(),
                        "{standalone}/{case}/{reopen}/{stage}"
                    );
                    let counts = Arc::new(PublicationIoCounts::default());
                    with_publication_io_hook(counts.clone(), || apply(&manager)).unwrap();
                    assert_eq!(counts.writes.load(Ordering::Relaxed), 1);
                    assert_eq!(
                        counts.full_syncs.load(Ordering::Relaxed),
                        if cfg!(unix) { 2 } else { 1 }
                    );
                    let counts = Arc::new(PublicationIoCounts::default());
                    with_publication_io_hook(counts.clone(), || apply(&manager)).unwrap();
                    assert_eq!(counts.writes.load(Ordering::Relaxed), 0);
                    assert_eq!(counts.syncs.load(Ordering::Relaxed), 0);
                }
            }
        }
    }
}

#[test]
fn equal_record_replaced_by_another_manager_needs_new_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).unwrap();
    manager
        .bind_and_activate_turn("p", "s", "r", "provider", true, false)
        .unwrap();
    let other = LockdownManager::initialize(root).unwrap();
    assert!(
        with_publication_io_fault(3, || other
            .bind_and_activate_turn("p", "s", "r", "provider", true, false))
        .is_err()
    );
    assert!(with_publication_io_fault(3, || manager.activate_turn("p", "s", "r")).is_err());
    manager.activate_turn("p", "s", "r").unwrap();
}

#[test]
fn fresh_process_confirms_a_record_left_visible_by_a_failed_save() {
    const TEST: &str =
        "lockdown_save_tests::fresh_process_confirms_a_record_left_visible_by_a_failed_save";
    if let Some(root) = std::env::var_os("HAIDER_TEST_UNCERTAIN_SETTINGS_ROOT") {
        let manager = LockdownManager::initialize(std::path::PathBuf::from(root)).unwrap();
        if std::env::var("HAIDER_TEST_UNCERTAIN_SETTINGS_STAGE").unwrap() == "fail" {
            assert!(
                with_publication_io_fault(3, || manager
                    .bind_and_activate_turn("p", "s", "r", "provider", true, false))
                .is_err()
            );
        } else {
            let counts = Arc::new(PublicationIoCounts::default());
            with_publication_io_hook(counts.clone(), || manager.activate_turn("p", "s", "r"))
                .unwrap();
            assert_eq!(counts.writes.load(Ordering::Relaxed), 1);
            assert_eq!(
                counts.full_syncs.load(Ordering::Relaxed),
                if cfg!(unix) { 2 } else { 1 }
            );
        }
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for stage in ["fail", "confirm"] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(
                "HAIDER_TEST_UNCERTAIN_SETTINGS_ROOT",
                temp.path().join("lockdown"),
            )
            .env("HAIDER_TEST_UNCERTAIN_SETTINGS_STAGE", stage)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{stage}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn save_identity_scope_and_hub_shutdown_do_not_fence_other_owners() {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().to_str().unwrap();
    let owners = SaveOwners::default();
    let other = SaveOwners::default();
    let s = SessionId::new("s");
    let r = RunId::new("r");
    let first = owners.begin(profile, &s, &r).unwrap();
    assert!(
        first
            .permit()
            .ensure_for(&SessionId::new("wrong"), &r)
            .is_err()
    );
    assert!(first.permit().ensure_for(&s, &RunId::new("wrong")).is_err());
    let unrelated = owners
        .begin(profile, &SessionId::new("other-session"), &r)
        .unwrap();
    let unrelated_owners = SaveOwners::default();
    let unrelated_profile = unrelated_owners.begin("other-profile", &s, &r).unwrap();
    first.permit().ensure_current().unwrap();
    unrelated.permit().ensure_current().unwrap();
    let successor = other.begin(profile, &s, &RunId::new("new")).unwrap();
    // A new hub takes over even before old-hub shutdown has finished.
    assert!(first.permit().ensure_current().is_err());
    owners.close();
    assert!(first.permit().ensure_current().is_err());
    assert!(unrelated.permit().ensure_current().is_err());
    successor.permit().ensure_current().unwrap();
    unrelated_profile.permit().ensure_current().unwrap();
    assert!(matches!(
        owners.begin(profile, &s, &r),
        Err(SessionHubError::Closed)
    ));
}
