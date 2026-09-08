#![allow(clippy::expect_used)]
use super::*;

#[test]
fn android_workspace_rejects_ancestors_reserved_paths_and_replaced_children() {
    let root = test_root();
    let child = tempfile::tempdir_in(root).expect("child");
    let outside = tempfile::tempdir().expect("outside");
    assert!(open(child.path()).is_ok());
    assert!(open(outside.path()).is_err());
    assert!(open(&root.join("../")).is_err());
    assert!(open(&root.join(".haider-lockdown")).is_err());
    let moved = outside.path().join("moved");
    std::fs::rename(child.path(), &moved).expect("move child");
    std::os::unix::fs::symlink(&moved, child.path()).expect("replace child with symlink");
    assert!(open(child.path()).is_err());
    std::fs::remove_file(child.path()).expect("remove synthetic link");
}

#[test]
fn android_provider_sandbox_is_inside_workspace_and_uses_anchored_io() {
    let root = test_root();
    let store = tempfile::tempdir().expect("bookkeeping");
    let manager = crate::lockdown::LockdownManager::initialize(store.path().join("lockdown"))
        .expect("manager");
    let provider = "android-sandbox-fixture";
    let sandbox = manager.provider_root(provider).expect("sandbox");
    assert!(sandbox.starts_with(root.join(".haider-lockdown")));
    assert!(!sandbox.starts_with(store.path()));
    manager
        .write(provider, Path::new("nested/data.txt"), b"synthetic initial")
        .expect("write");
    manager
        .write(
            provider,
            Path::new("nested/data.txt"),
            b"synthetic replacement",
        )
        .expect("replace");
    assert_eq!(
        manager
            .read(provider, Path::new("nested/data.txt"))
            .expect("read"),
        b"synthetic replacement"
    );
    assert!(
        manager
            .write(provider, Path::new("../escape"), b"no")
            .is_err()
    );
    let outside = tempfile::tempdir().expect("outside");
    let sentinel = outside.path().join("sentinel");
    std::fs::write(&sentinel, b"untouched").expect("sentinel");
    std::os::unix::fs::symlink(outside.path(), sandbox.join("link")).expect("symlink");
    assert!(
        manager
            .write(provider, Path::new("link/sentinel"), b"no")
            .is_err()
    );
    assert!(manager.read(provider, Path::new("link/sentinel")).is_err());
    assert_eq!(std::fs::read(&sentinel).expect("sentinel"), b"untouched");
    std::fs::remove_file(sandbox.join("link")).expect("remove link");
    std::fs::hard_link(&sentinel, sandbox.join("hard")).expect("hard link");
    assert!(manager.read(provider, Path::new("hard")).is_err());
    assert!(manager.write(provider, Path::new("hard"), b"no").is_err());
    std::fs::remove_file(sandbox.join("hard")).expect("remove hard link");
    std::fs::remove_dir_all(sandbox).expect("remove synthetic sandbox");
}
