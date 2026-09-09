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

#[test]
fn android_checkpoint_restore_retains_authority_and_rejects_reserved_subtrees() {
    use haider_tools::{
        CheckpointRestorePlan, CheckpointRestoreTarget, restore_checkpoint_plan_anchored,
    };
    use std::sync::Arc;
    let child = tempfile::tempdir_in(test_root()).expect("workspace");
    let authority = Arc::new(open(child.path()).expect("retained ceiling descendant"));
    let mut plan = CheckpointRestorePlan {
        workspace_root: child.path().into(),
        targets: vec![CheckpointRestoreTarget {
            path: "deleted.txt".into(),
            expected_digest: None,
            restore_bytes: Some(b"synthetic checkpoint".to_vec()),
        }],
    };
    let capture =
        restore_checkpoint_plan_anchored(&plan, authority.clone()).expect("undo deletion");
    assert_eq!(
        std::fs::read(child.path().join("deleted.txt")).expect("restored"),
        b"synthetic checkpoint"
    );
    plan.targets[0].expected_digest = capture[0].post_digest.clone();
    plan.targets[0].restore_bytes = None;
    restore_checkpoint_plan_anchored(&plan, authority.clone()).expect("redo deletion");
    assert!(!child.path().join("deleted.txt").exists());
    for path in [".haider-lockdown/sentinel", "../sentinel"] {
        plan.targets[0].path = path.into();
        plan.targets[0].expected_digest = None;
        plan.targets[0].restore_bytes = Some(b"forbidden".to_vec());
        assert!(restore_checkpoint_plan_anchored(&plan, authority.clone()).is_err());
    }
    // Model a journal failure after two successful publications. Recovery
    // must restore their preimages even if the cwd pathname is replaced next.
    for name in ["deleted.txt", "second.txt"] {
        std::fs::write(child.path().join(name), b"original").expect("preimage");
    }
    plan.targets = ["deleted.txt", "second.txt"]
        .into_iter()
        .map(|name| CheckpointRestoreTarget {
            path: name.into(),
            expected_digest: Some(format!("blake3:{}", blake3::hash(b"original").to_hex())),
            restore_bytes: Some(b"applied before journal failure".to_vec()),
        })
        .collect();
    let applied =
        restore_checkpoint_plan_anchored(&plan, authority.clone()).expect("apply both targets");
    let recovery = CheckpointRestorePlan {
        workspace_root: plan.workspace_root.clone(),
        targets: applied
            .iter()
            .map(|capture| CheckpointRestoreTarget {
                path: capture.path.clone(),
                expected_digest: capture.post_digest.clone(),
                restore_bytes: capture.pre_bytes.clone(),
            })
            .collect(),
    };
    let old = child.path().with_extension("retained");
    std::fs::rename(child.path(), &old).expect("rename cwd after publication");
    let foreign = tempfile::tempdir().expect("foreign target");
    std::os::unix::fs::symlink(foreign.path(), child.path()).expect("swapped ancestor");
    restore_checkpoint_plan_anchored(&recovery, authority.clone())
        .expect("journal-failure recovery through retained directory");
    for name in ["deleted.txt", "second.txt"] {
        assert!(!foreign.path().join(name).exists());
        assert_eq!(
            std::fs::read(old.join(name)).expect("recovered preimage"),
            b"original"
        );
    }
    plan.targets = vec![CheckpointRestoreTarget {
        path: "absent.txt".into(),
        expected_digest: None,
        restore_bytes: Some(b"retained".to_vec()),
    }];
    restore_checkpoint_plan_anchored(&plan, authority).expect("create using retained directory");
    assert!(!foreign.path().join("absent.txt").exists());
    assert_eq!(
        std::fs::read(old.join("absent.txt")).expect("retained target"),
        b"retained"
    );
    std::fs::remove_file(child.path()).expect("remove synthetic symlink");
    std::fs::remove_dir_all(old).expect("remove retained fixture");
}
