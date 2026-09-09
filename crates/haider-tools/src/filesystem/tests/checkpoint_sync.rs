#![allow(clippy::expect_used)]

use crate::checkpoint::{
    CheckpointRestorePlan, CheckpointRestoreTarget, restore_checkpoint_plan,
    restore_checkpoint_plan_anchored,
};
use haider_platform::SyncPolicy;
use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

type ParentSync = (u64, u64, SyncPolicy);

thread_local! {
    static OBSERVATIONS: RefCell<Option<Vec<ParentSync>>> = const { RefCell::new(None) };
}

// Observe the actual call's descriptor and policy, then execute the platform
// operation unchanged. On Apple this still performs the real F_FULLFSYNC.
pub(super) fn sync_parent(file: &fs::File, policy: SyncPolicy) -> std::io::Result<()> {
    OBSERVATIONS.with(|slot| -> std::io::Result<()> {
        if let Some(observations) = slot.borrow_mut().as_mut() {
            let metadata = file.metadata()?;
            observations.push((metadata.dev(), metadata.ino(), policy));
        }
        Ok(())
    })?;
    haider_platform::sync_file(file, policy)
}

fn observe(action: impl FnOnce()) -> Vec<ParentSync> {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            OBSERVATIONS.with(|slot| slot.replace(None));
        }
    }
    OBSERVATIONS.with(|slot| assert!(slot.replace(Some(Vec::new())).is_none()));
    let _reset = Reset;
    action();
    OBSERVATIONS.with(|slot| slot.replace(None).expect("active sync observation"))
}

#[test]
fn checkpoint_sync_preserves_full_durability_for_desktop_create_replace_and_delete() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let parent = root.path().join("nested");
    fs::create_dir(&parent).expect("create checkpoint parent");
    let metadata = fs::metadata(&parent).expect("parent identity");
    let mut previous: Option<Vec<u8>> = None;
    for desired in [Some(b"before".to_vec()), Some(b"after".to_vec()), None] {
        let plan = CheckpointRestorePlan {
            workspace_root: root.path().to_path_buf(),
            targets: vec![CheckpointRestoreTarget {
                path: "nested/target.txt".into(),
                expected_digest: previous.as_deref().map(super::mutation_digest),
                restore_bytes: desired.clone(),
            }],
        };
        let observations = observe(|| {
            restore_checkpoint_plan(&plan).expect("ordinary desktop checkpoint restore");
        });
        assert_eq!(
            observations,
            vec![(metadata.dev(), metadata.ino(), SyncPolicy::Full)],
            "every publication, including delete-only restore, must flush its parent with Full"
        );
        assert_eq!(fs::read(parent.join("target.txt")).ok(), desired);
        previous = desired;
    }
}

#[test]
fn checkpoint_sync_keeps_the_retained_parent_after_workspace_rename() {
    let root = tempfile::tempdir().expect("temporary root");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(workspace.join("nested")).expect("create retained workspace");
    fs::write(workspace.join("nested/target.txt"), b"before").expect("seed retained file");
    let directory = Arc::new(
        haider_platform::open_workspace_directory(&workspace).expect("retain workspace authority"),
    );
    let moved = root.path().join("moved");
    fs::rename(&workspace, &moved).expect("rename retained workspace");
    fs::create_dir_all(workspace.join("nested")).expect("replace displayed workspace path");
    fs::write(workspace.join("nested/target.txt"), b"replacement").expect("seed replacement");
    let metadata = fs::metadata(moved.join("nested")).expect("retained parent identity");
    let plan = CheckpointRestorePlan {
        workspace_root: workspace.clone(),
        targets: vec![CheckpointRestoreTarget {
            path: "nested/target.txt".into(),
            expected_digest: Some(super::mutation_digest(b"before")),
            restore_bytes: Some(b"restored".to_vec()),
        }],
    };
    let observations = observe(|| {
        restore_checkpoint_plan_anchored(&plan, directory).expect("anchored checkpoint restore");
    });
    assert_eq!(
        observations,
        vec![(metadata.dev(), metadata.ino(), SyncPolicy::Full)]
    );
    assert_eq!(
        fs::read(moved.join("nested/target.txt")).expect("retained result"),
        b"restored"
    );
    assert_eq!(
        fs::read(workspace.join("nested/target.txt")).expect("replacement untouched"),
        b"replacement"
    );
}
