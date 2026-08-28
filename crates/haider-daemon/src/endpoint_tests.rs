#![allow(clippy::expect_used)]

use crate::DaemonError;
use crate::endpoint::RuntimeDirectory;

const JOURNAL_CHILD_ENV: &str = "HAIDER_ENDPOINT_JOURNAL_CHILD";

#[test]
fn unowned_runtime_leftover_is_retained_with_a_truthful_journal_line() {
    if std::env::var_os(JOURNAL_CHILD_ENV).is_none() {
        let output = std::process::Command::new(
            std::env::current_exe().expect("current test binary"),
        )
        .args([
            "--exact",
            "endpoint_tests::unowned_runtime_leftover_is_retained_with_a_truthful_journal_line",
            "--nocapture",
        ])
        .env(JOURNAL_CHILD_ENV, "1")
        .output()
        .expect("run isolated journal child");
        assert!(
            output.status.success(),
            "journal child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(
                "step=remove profile runtime directory \
                 outcome=retained_unowned_entries \
                 reason=no_remaining_entry_is_daemon_owned"
            ),
            "cleanup did not emit the truthful retention outcome: {stderr}"
        );
        assert!(stderr.contains("retained_entries=["));
        assert!(stderr.contains("store"));
        return;
    }

    let root = tempfile::tempdir().expect("runtime fixture");
    let runtime_path = root.path().join("runtime");
    let mut runtime = RuntimeDirectory::prepare(&runtime_path).expect("prepare runtime");
    let store = runtime_path.join("store");
    std::fs::create_dir_all(&store).expect("durable store directory");
    std::fs::write(store.join("accounts.json"), b"[]").expect("durable store data");

    runtime
        .cleanup()
        .expect("an unowned store must not fail daemon cleanup");

    assert!(store.join("accounts.json").is_file());
}

#[test]
fn owned_runtime_leftover_remains_a_typed_error_with_entries() {
    let root = tempfile::tempdir().expect("runtime fixture");
    let runtime_path = root.path().join("runtime");
    let mut runtime = RuntimeDirectory::prepare(&runtime_path).expect("prepare runtime");
    let store = runtime_path.join("store");
    let owned_pid = runtime_path.join("haiderd.pid");
    std::fs::create_dir(&store).expect("unowned store");
    std::fs::write(store.join("accounts.json"), b"[]").expect("unowned store data");
    std::fs::write(&owned_pid, b"owned residual").expect("owned residual");
    runtime.remember_owned_path(owned_pid.clone());

    let error = runtime
        .cleanup()
        .expect_err("a daemon-owned residual must fail cleanup after bounded retry");
    match error {
        DaemonError::RuntimeDirectoryNotEmpty {
            path,
            mut remaining_entries,
        } => {
            remaining_entries.sort();
            let mut expected = vec![owned_pid.clone(), store.clone()];
            expected.sort();
            assert_eq!(path, runtime_path);
            assert_eq!(remaining_entries, expected);
        }
        other => panic!("expected typed runtime residual, got {other:?}"),
    }

    assert!(store.join("accounts.json").is_file());
    std::fs::remove_file(owned_pid).expect("release owned residual");
    std::fs::remove_file(store.join("accounts.json")).expect("release store data");
    std::fs::remove_dir(store).expect("release store");
    runtime.cleanup().expect("cleanup after releasing residual");
}
