# Wave 973 integration, Sol round 16

Merged `origin/wave-973` at `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the model list fixes lane.

`crates/haider-daemon/src/android_runtime_tests.rs` conflicted because the lane had repaired the recovered-checkpoint fixture's effect history while the wave had extracted that fixture into `checkpoint_test_support.rs`. The shared helper already includes the same ordered Intent → Authorized → Dispatched → Outcome → CheckpointRecorded history, and asserts that the checkpoint is indexed. The resolution uses that shared helper and removes the duplicate local definition. It retains the lane's recovered Android RPC assertions for allowed paths, outside/reserved paths, worker generation, undo, redo, and rollback, along with the wave's general support for creation/write/deletion fixtures and durable postimages.

`test-baseline.txt` was derived from the merged source and the new regression tests using `cargo run --locked --offline -p xtask -- test-count --update`. The resulting count is **6,774**. Neither parent's number was chosen. The round 16 `integrated-baseline-update.log` records the actual command and exit 0.

The imported Android application changes are part of the wave. The model list repair adds no Android application UI change. Its standalone Rust feature, the shared fixture, and the full workspace gates are checked on the merged candidate.
