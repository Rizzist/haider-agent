#![allow(clippy::expect_used)]

use crate::loom_seed::seed_loom_registry;
use haider_core::SqliteStoreHandle;

/// MUTATION CHECK: replace the `is_none` gate in `seed_loom_registry` with
/// unconditional registration. Expected runtime failure: the revised job
/// below is clobbered back to the seed on the second pass.
#[tokio::test]
async fn seeding_is_absent_only_and_never_clobbers_a_user_revision() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    seed_loom_registry(&store).await.expect("first seed");
    let scout = store
        .loom_agent_type("scout".into())
        .await
        .expect("read")
        .expect("scout seeded");
    assert_eq!(scout.rev, 1, "the registry owns revs");
    assert_eq!(scout.color, "#7aa2f7");
    assert_eq!(scout.glyph, "⌖");
    let reviewer = store
        .loom_agent_type("reviewer".into())
        .await
        .expect("read")
        .expect("reviewer seeded");
    assert_eq!(reviewer.color, "#bb9af7");

    let mut revised = scout.clone();
    revised.job = "my own scout brief".into();
    store
        .loom_register_agent_type(revised)
        .await
        .expect("user revision");
    seed_loom_registry(&store).await.expect("re-seed");
    let kept = store
        .loom_agent_type("scout".into())
        .await
        .expect("read")
        .expect("still present");
    assert_eq!(kept.job, "my own scout brief", "user edits outlive seeds");
    assert_eq!(kept.rev, 2);
    store.close().await.expect("close");
}
