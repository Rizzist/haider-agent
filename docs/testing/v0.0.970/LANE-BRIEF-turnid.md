# Lane turnid — CONTINUATION: the full workspace test fails on your own goldens (v0.0.970)
Your implementation is COMMITTED on lane-970-turnid and the merge-forward onto wave-970 is committed too (registry-walk conflict already
resolved by the orchestrator — both delta walks kept). But your verdict was reached with `cargo check --workspace --all-targets`, not
`cargo test --workspace`, and the landing chain's full workspace gate FAILS with 4 tests in 3 suites:
  crates/haider-cli/tests/oneshot_boot_tests.rs:515  one_shot_jsonl_stream_matches_the_normalized_golden
  crates/haider-cli/tests/turnhygiene_pin_tests.rs:446  run_jsonl_text_turn_matches_the_normalized_golden
  crates/haider-cli/tests/turnhygiene_pin_tests.rs:446  run_jsonl_tool_turn_matches_the_normalized_golden
  crates/haider-core/tests/runtime_tests.rs:744  full_turn_commits_exact_projected_sequence
  crates/haider-daemon: turn_recovery::composite_recovery_tests::pending_cancellation_handoff_suppresses_workflow_continuation_shape
  crates/haider-daemon: worker::cu1_image_runtime_tests::daemon_compactor_fuses_provider_view_and_cache_attempt_publication
Cause (verify it): your new `correlation` object ({request_kind, request_ordinal, run_id, session_id, turn_ordinal}) is now serialized into
the provider_request_attempt payload, so every pinned JSONL/projection golden that captures that payload drifted. Example diff at char 336
of the first differing line: golden goes `..."prompt":"omit"},"payload":{"event":"started","item":{"data":{"diagnostic":{...` while actual
now has `..."data":{"correlation":{"request_kind":"primary","request_ordinal":1,...},"di...`.
Task: REVIEW each drifted golden line by line and confirm the ONLY change is the additive correlation object in the expected position (no
field lost, no ordering churn beyond the new key, no value that should have been redacted — correlation must carry opaque IDs and ordinals
only, never prompt/body/credential/path/error text). Then re-bless deliberately (UPDATE_FIXTURES=1 for the fixture goldens; fix the two
daemon tests and the core projection test in whatever way is correct — updating an expectation is fine when the new shape is intended, but
say per test which you did and why). Then run the FULL gate exactly as the landing chain does: `cargo test -q --workspace --no-fail-fast`
and `cargo clippy --workspace --tests -- -D warnings` under the ENV LAW (RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 HAIDER_TEST_SIBLINGS_PREBUILT=1), plus the repo's
test-count update. Do not commit (the worktree git dir is outside your sandbox) — leave the tree ready and STOP; the orchestrator commits.
Report per-golden what changed and why it is correct, the full-gate totals, clippy exit, and the baseline. LAST line SHIP or NO_SHIP.
