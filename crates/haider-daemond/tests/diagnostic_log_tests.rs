//! End-to-end pins for the daemon process-log route.
//!
//! These tests launch the real `haiderd` binary through the production
//! `haider_platform::spawn_daemon` path. File allocation alone is not the
//! assertion: a real lock-contention diagnostic must arrive intact.

#![allow(clippy::expect_used)]

use haider_platform::{DaemonSpawn, allocate_daemon_log_path, spawn_daemon_with_machine_user_home};
use haider_store::Store;
use std::path::{Path, PathBuf};
use std::process::Child;

const ALREADY_RUNNING_EXIT: i32 = 75;

fn spawn_contender(store_dir: &Path, runtime_dir: &Path, profile_id: &str) -> (Child, PathBuf) {
    let log_path = allocate_daemon_log_path(store_dir).expect("allocate per-process daemon log");
    let machine_user_home = store_dir.parent().expect("store has a test root");
    let child = spawn_daemon_with_machine_user_home(
        DaemonSpawn {
            binary: Path::new(env!("CARGO_BIN_EXE_haiderd")),
            profile_id,
            store_dir,
            runtime_dir,
            log_path: &log_path,
        },
        machine_user_home,
    )
    .expect("spawn real daemon contender");
    (child, log_path)
}

fn wait_for_contention(mut child: Child) {
    let status = child.wait().expect("wait for daemon contender");
    assert_eq!(status.code(), Some(ALREADY_RUNNING_EXIT));
}

/// MUTATION PIN (diagnostic route): in `haider_platform::spawn_daemon`, open
/// `spec.store_dir.join(DAEMON_LOG_FILE)` instead of `spec.log_path`. The real
/// contention event then lands in the legacy file and this content assertion
/// observes an empty per-process file.
#[test]
fn real_daemon_diagnostic_event_reaches_nonempty_per_process_log() {
    let root = tempfile::tempdir().expect("tempdir");
    let store_dir = root.path().join("store");
    let runtime_dir = root.path().join("runtime");
    let _lease = Store::acquire_profile(&store_dir).expect("hold profile lock");
    let profile_id = "diagnostic-event-pin";

    let (child, log_path) = spawn_contender(&store_dir, &runtime_dir, profile_id);
    wait_for_contention(child);

    let content = std::fs::read_to_string(&log_path).expect("read per-process daemon log");
    assert!(
        !content.is_empty(),
        "a real diagnostic event must write bytes"
    );
    assert!(
        content.contains(&format!(
            "haiderd: profile `{profile_id}` is already running"
        )),
        "per-process log omitted the real contention event: {content:?}"
    );
}

/// MUTATION PIN (process isolation): return `store_dir.join(DAEMON_LOG_FILE)`
/// from `allocate_daemon_log_path`. Both real contenders then report the same
/// path instead of owning distinct files, and this test fails before treating
/// a shared writer as valid evidence.
#[test]
fn concurrent_real_daemons_have_distinct_logs_with_intact_lines() {
    let root = tempfile::tempdir().expect("tempdir");
    let store_dir = root.path().join("store");
    let runtime_dir = root.path().join("runtime");
    let _lease = Store::acquire_profile(&store_dir).expect("hold profile lock");
    let profiles = ["concurrent-diagnostic-a", "concurrent-diagnostic-b"];

    // Reproduce an established profile rather than the first-write race in
    // the legacy contention limiter: every observed zero-byte file was
    // allocated while this per-store suppression window was already active.
    let (warmup, warmup_path) = spawn_contender(&store_dir, &runtime_dir, "contention-warmup");
    wait_for_contention(warmup);
    assert!(
        std::fs::metadata(warmup_path)
            .expect("warmup diagnostic metadata")
            .len()
            > 0,
        "warmup must establish the contention diagnostic window"
    );

    let contenders = profiles
        .iter()
        .map(|profile| spawn_contender(&store_dir, &runtime_dir, profile))
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    for ((child, path), profile) in contenders.into_iter().zip(profiles) {
        wait_for_contention(child);
        paths.push((path, profile));
    }

    assert_ne!(paths[0].0, paths[1].0, "each daemon must own its log path");
    for (path, profile) in &paths {
        let content = std::fs::read_to_string(path).expect("read isolated daemon log");
        let lines = content.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            1,
            "{} must contain one complete diagnostic line: {content:?}",
            path.display()
        );
        assert!(content.ends_with('\n'), "diagnostic line must be complete");
        assert!(
            lines[0].contains(&format!("profile `{profile}` is already running")),
            "{} contains a foreign or partial line: {content:?}",
            path.display()
        );
        assert!(
            profiles
                .iter()
                .filter(|candidate| lines[0].contains(**candidate))
                .eq(std::iter::once(profile)),
            "{} contains interleaved process markers: {content:?}",
            path.display()
        );
    }
}
