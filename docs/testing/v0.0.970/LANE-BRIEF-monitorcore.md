# Lane monitorcore — CONTINUATION 3: merge-forward onto wave-970 and resolve conflicts (v0.0.970)
Your implementation is COMMITTED on this branch (24 files, +7346/-1232, SHIP with an independent verifier). The landing chain failed to
merge `origin/wave-970` (head c318d7b5: sessionloss and voicefix landed after your base) into this branch: conflicts in
crates/haider-daemon/src/connection_tests.rs, crates/haider-rpc/src/lib.rs (feature-flag / export lists — sessionloss added
`session_list_recency_v1`; you added the monitor RPC features), test-baseline.txt. The merge was aborted; the tree is clean.
Task: `git fetch origin wave-970 && git merge --no-commit origin/wave-970`; resolve preserving BOTH sides (keep every feature flag, export,
and test from both; test-baseline.txt = recount with the repo's test-count tool). You cannot commit (worktree git dir is outside your
sandbox) — leave the resolved merge in the working tree and STOP after verification; the orchestrator commits. Verify with the ENV LAW:
`cargo test -p haider-rpc -p haider-daemon -p haider-daemond -p haider-tools -p haider-store -p haider-platform` and
`cargo clippy --workspace --tests -- -D warnings`; fix semantic conflicts the merge introduces. Report per-file resolution, suite totals,
clippy exit, the new baseline. LAST line SHIP or NO_SHIP.
