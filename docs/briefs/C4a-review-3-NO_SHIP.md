# C4a review round 3 — NO_SHIP (gpt-5.6, frozen 13c089e)

Verified clean `w2-c4a` at `13c089e`.

### Findings

1. **P1 — Cancellation plus ledger failure can still leave a silent write.** The rename and ledger call share one blocking worker, but the failed result is journaled only after awaiting that worker (`crates/haider-tools/src/filesystem.rs:358`). If the outer task is cancelled after rename while `record_fs_write` subsequently returns an error, the worker completes into a dropped join handle and `finish` never records the failed outcome (`crates/haider-tools/src/filesystem.rs:379`). The disk change then has neither ledger evidence nor an outcome. Existing tests cover cancellation with a successful ledger and ledger failure without cancellation, but not their intersection (`crates/haider-tools/tests/filesystem_tools_tests.rs:334`).

The fd-anchored `O_NOFOLLOW` walk and typed symlink-swap refusal are otherwise correct. Same-fd preimage verification, current-inode locking, anchored temp creation, and `renameat` correctly serialize broker-mediated writes. The documented non-broker-writer residual accurately matches the stated §9.2 external-edit contract.

Formatting and the 99-test repository guard passed. Full tests were blocked by the read-only environment denying Cargo's lock and temporary-directory creation; the worktree remained clean.

VERDICT: NO_SHIP
