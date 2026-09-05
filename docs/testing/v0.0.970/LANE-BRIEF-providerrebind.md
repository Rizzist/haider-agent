# Lane providerrebind — CONTINUATION: clippy under --tests fails on the merged tree (v0.0.970)
Your implementation and the real merge onto wave-970 are COMMITTED on lane-970-providerrebind (HEAD includes the merge + the
SessionMetadataV1 test-initializer completion). The landing chain's full workspace gate PASSED (5,302 tests) but
`cargo clippy --workspace --tests -- -D warnings` FAILS: `field assignment outside of initializer for an instance created with
Default::default()` (clippy::field_reassign_with_default) at crates/haider-cli/src/session_provider.rs:71, reported while compiling the
cli_tests target. Fix it the idiomatic way (struct literal with `..Default::default()`), then re-run EXACTLY the landing gate under the ENV
LAW: `cargo test -q --workspace --no-fail-fast` and `cargo clippy --workspace --tests -- -D warnings` (with --tests — verbatim), plus the
test-count update; fix anything else the --tests clippy pass reports. Do not commit (worktree git dir outside your sandbox) — leave the tree
ready and STOP. Report the fix, full-gate totals, clippy exit, the baseline, and the MANDATORY `VERIFIER: findings=<n> real=<n> noise=<n> — …`.
LAST line SHIP or NO_SHIP.
