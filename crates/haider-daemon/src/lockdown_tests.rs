#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use super::lockdown::{LockdownError, LockdownManager, provider_slug, read_path_allowed};

#[test]
fn quota_is_global_across_providers_and_reconciles_on_start() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    manager.set_quota(12).expect("quota");
    manager
        .write("one", Path::new("one.txt"), b"12345678")
        .expect("first write");
    let error = manager
        .write("two", Path::new("two.txt"), b"12345")
        .expect_err("shared quota must refuse the second provider");
    assert!(matches!(
        error,
        LockdownError::LockdownQuotaExceeded { used: 8, limit: 12 }
    ));

    let first_root = manager.provider_root("one").expect("provider root");
    fs::write(first_root.join("drift"), b"12").expect("simulate a crash after data publication");
    let restarted = LockdownManager::initialize(root).expect("restart manager");
    assert_eq!(
        restarted.status(None).expect("status").quota_used,
        10,
        "startup must reconcile the ledger against real files"
    );
}

/// MUTATION CHECK: restore the unconditional ledger replacement. Expected
/// runtime failure: an unchanged startup receives a new inode and mtime.
#[test]
fn unchanged_startup_scan_does_not_rewrite_the_quota_ledger() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    drop(manager);
    let path = root.join("quota.json");
    let before = fs::metadata(&path).expect("ledger metadata before restart");
    let before_modified = before.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let restarted = LockdownManager::initialize(root).expect("unchanged restart");
    drop(restarted);
    let after = fs::metadata(path).expect("ledger metadata after restart");
    assert_eq!(
        before_modified,
        after.modified().unwrap_or(SystemTime::UNIX_EPOCH)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(before.ino(), after.ino());
    }
}

#[test]
fn replacing_a_file_accounts_for_only_the_new_size() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager.set_quota(8).expect("quota");
    manager
        .write("provider", Path::new("answer.txt"), b"123456")
        .expect("initial write");
    let status = manager
        .write("provider", Path::new("answer.txt"), b"12")
        .expect("replacement");
    assert_eq!(status.quota_used, 2);
    let status = manager
        .write("provider", Path::new("quota.json"), b"123")
        .expect("a sandbox file cannot masquerade as the global ledger");
    assert_eq!(status.quota_used, 5);
}

#[test]
fn replacement_staging_cannot_exceed_the_physical_quota_peak() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager.set_quota(6).expect("quota");
    manager
        .write("provider", Path::new("answer.txt"), b"123456")
        .expect("initial write");
    assert!(matches!(
        manager
            .write("provider", Path::new("answer.txt"), b"12")
            .expect_err("staged replacement would exceed the physical peak"),
        LockdownError::LockdownQuotaExceeded { used: 6, limit: 6 }
    ));
}

#[test]
fn quota_commands_replay_exactly_and_refuse_coordinate_reuse() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    manager
        .set_quota_command("quota-command", 12)
        .expect("first command");
    manager
        .set_quota_command("later-command", 15)
        .expect("intervening quota command");
    let replay = LockdownManager::initialize(root)
        .expect("restart")
        .set_quota_command("quota-command", 12)
        .expect("exact replay");
    assert_eq!(replay.quota_limit, 12);
    assert_eq!(replay.quota_used, 0);
    assert!(matches!(
        manager
            .set_quota_command("quota-command", 13)
            .expect_err("command reuse must be refused"),
        LockdownError::QuotaCommandConflict { ref command_id }
            if command_id == "quota-command"
    ));
}

#[test]
fn quota_cannot_be_lowered_below_reconciled_use() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager
        .write("research", Path::new("answer.txt"), b"1234")
        .expect("sandbox write");
    assert!(matches!(
        manager
            .set_quota(3)
            .expect_err("quota cannot be lower than real use"),
        LockdownError::LockdownQuotaExceeded { used: 4, limit: 3 }
    ));
}

/// MUTATION CHECK: remove stale-temp cleanup before ledger load. Expected
/// failure: restart cannot recreate the fixed atomic ledger temporary.
#[test]
fn restart_recovers_private_ledger_and_data_temporaries() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    manager.set_quota(64).expect("quota");
    fs::write(root.join("quota.tmp"), b"interrupted ledger").expect("stale ledger temp");
    let data_temp = root.join(".ld-0123456789abcdef");
    fs::write(&data_temp, b"interrupted data").expect("stale data temp");

    let restarted = LockdownManager::initialize(root.clone()).expect("restart after crash");
    assert!(!root.join("quota.tmp").exists());
    assert!(!data_temp.exists());
    assert_eq!(restarted.status(None).expect("status").quota_used, 0);
}

/// MUTATION CHECK: create provider descendants before quota admission.
/// Expected failure: a refused nested write leaves an observable directory.
#[test]
fn quota_refusal_has_no_provider_directory_side_effect() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager.set_quota(0).expect("zero quota");
    let provider_root = manager.provider_root("research").expect("provider root");
    assert!(matches!(
        manager
            .write("research", Path::new("nested/result.txt"), b"x")
            .expect_err("quota refusal"),
        LockdownError::LockdownQuotaExceeded { used: 0, limit: 0 }
    ));
    assert!(!provider_root.exists());
}

/// MUTATION CHECK: keep turn bindings only in `SessionHub` memory or accept
/// a second provider for the same run. Expected failure: restart widens the
/// existing run or returns a mismatched provider coordinate.
#[test]
fn turn_binding_survives_restart_and_rejects_provider_mismatch() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("lockdown");
    let manager = LockdownManager::initialize(root.clone()).expect("manager");
    assert_eq!(
        manager
            .bind_turn("profile", "session", "run", "research", true)
            .expect("first bind"),
        ("research".to_owned(), true)
    );
    assert_eq!(
        manager
            .active_session_binding("profile", "session")
            .expect("inactive binding lookup"),
        None,
        "a queued/bound run must not become the session ceiling before activation"
    );
    manager
        .activate_turn("profile", "session", "run")
        .expect("activate first bind");
    let restarted = LockdownManager::initialize(root).expect("restart");
    assert_eq!(
        restarted
            .bind_turn("profile", "session", "run", "research", false)
            .expect("durable bind wins over mutable trust"),
        ("research".to_owned(), true)
    );
    assert!(matches!(
        restarted
            .bind_turn("profile", "session", "run", "other", false)
            .expect_err("same run cannot change provider"),
        LockdownError::TurnBindingConflict { .. }
    ));
    assert_eq!(
        restarted
            .latest_session_provider("profile", "session")
            .expect("latest provider"),
        Some("research".to_owned())
    );
    assert_eq!(
        restarted
            .active_session_binding("profile", "session")
            .expect("active binding"),
        Some(("run".to_owned(), "research".to_owned(), true))
    );
}

#[test]
fn sandbox_rejects_parent_escape_and_sensitive_reads() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    let sandbox = fixture.path().join("lockdown/provider");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&sandbox).expect("sandbox");

    assert!(read_path_allowed(
        &workspace,
        &sandbox,
        Path::new("src/lib.rs")
    ));
    assert!(!read_path_allowed(
        &workspace,
        &sandbox,
        &workspace.join("../.ssh/id_ed25519")
    ));
    for sensitive in [
        ".env",
        ".env.production",
        ".haider/providers.json",
        "vault/credentials.json",
    ] {
        assert!(
            !read_path_allowed(&workspace, &sandbox, Path::new(sensitive)),
            "lockdown must refuse {sensitive}"
        );
    }

    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    let error = manager
        .write("provider", Path::new("../escape"), b"no")
        .expect_err("parent traversal must be refused");
    assert!(matches!(error, LockdownError::InvalidRelativePath { .. }));
}

#[test]
fn manager_rejects_native_outside_paths_before_creating_any_target() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    let provider_root = manager.provider_root("provider").expect("provider root");
    let absolute_outside = fixture.path().join("absolute-outside.txt");
    let rooted_name = format!(
        "haider-lockdown-rooted-{}",
        fixture
            .path()
            .file_name()
            .expect("fixture basename")
            .to_string_lossy()
    );
    let rooted_outside = Path::new(std::path::MAIN_SEPARATOR_STR).join(rooted_name);
    let parent_outside = provider_root
        .parent()
        .expect("provider root parent")
        .join("parent-outside.txt");

    for (label, requested) in [
        ("absolute outside", absolute_outside.as_path()),
        ("rooted", rooted_outside.as_path()),
        ("parent traversal", Path::new("../parent-outside.txt")),
    ] {
        let error = manager
            .write("provider", requested, b"no")
            .expect_err("outside write must be refused");
        assert!(
            matches!(error, LockdownError::InvalidRelativePath { .. }),
            "{label} must return InvalidRelativePath, got {error:?}"
        );
    }

    for path in [&absolute_outside, &rooted_outside, &parent_outside] {
        assert!(
            !path.exists(),
            "refused write must not create {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn workspace_symlink_cannot_alias_a_sensitive_read() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    let sandbox = fixture.path().join("lockdown/provider");
    let sensitive = workspace.join(".haider/providers.json");
    fs::create_dir_all(sensitive.parent().expect("sensitive parent")).expect("profile directory");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::write(&sensitive, b"secret").expect("sensitive fixture");
    symlink(&sensitive, workspace.join("innocent.txt")).expect("read alias");

    assert!(!read_path_allowed(
        &workspace,
        &sandbox,
        Path::new("innocent.txt")
    ));
}

#[test]
fn sandbox_lists_and_reads_only_its_own_written_files() {
    let fixture = tempfile::tempdir().expect("fixture");
    let manager = LockdownManager::initialize(fixture.path().join("lockdown")).expect("manager");
    manager
        .write("research", Path::new("notes/result.txt"), b"safe result")
        .expect("sandbox write");
    assert_eq!(
        manager
            .read("research", Path::new("notes/result.txt"))
            .expect("sandbox read"),
        b"safe result"
    );
    assert_eq!(
        manager
            .read("research", Path::new("notes"))
            .expect("sandbox list"),
        b"result.txt"
    );
}

#[test]
fn over_budget_paths_fail_typed_before_directory_creation() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("x".repeat(240));
    let error = LockdownManager::initialize(root.clone()).expect_err("path budget");
    assert!(matches!(
        error,
        LockdownError::PathTooLong { length, limit, .. } if length > limit
    ));
    assert!(!root.exists(), "validation must run before creation");
}

#[test]
fn root_budget_reserves_the_longest_provider_basename_before_creation() {
    let fixture = tempfile::tempdir().expect("fixture");
    let base = fixture.path().to_string_lossy().len();
    let remaining = 240_usize.saturating_sub(base + 1);
    let root = fixture.path().join("x".repeat(remaining));
    assert!(root.as_os_str().as_encoded_bytes().len() <= 240);
    let error = LockdownManager::initialize(root.clone()).expect_err("worst-case child budget");
    assert!(matches!(error, LockdownError::PathTooLong { .. }));
    assert!(!root.exists(), "child budget validation precedes creation");
}

#[test]
fn provider_sandbox_basename_stays_within_twenty_bytes() {
    let slug = provider_slug("an-extremely-long-provider-name").expect("provider slug");
    assert!(slug.len() <= 20, "{slug}");
    let slug = provider_slug("提供者").expect("non-ASCII provider slug");
    assert!(slug.len() <= 20, "{slug}");
}
