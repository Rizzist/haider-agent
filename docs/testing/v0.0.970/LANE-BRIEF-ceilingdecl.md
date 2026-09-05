# Lane ceilingdecl — CONTINUATION 2: the merge is STARTED for you; resolve it in the working tree (v0.0.970)
The orchestrator has run `git merge --no-commit origin/wave-970` (38359fd3) in this worktree: MERGE_HEAD is set and the conflicted
files carry <<<<<<< / ======= / >>>>>>> markers in the WORKING TREE. You need NO git metadata access: edit files only. Do not run git
merge/commit/checkout/reset; `git diff`, `git status`, `git diff --name-only --diff-filter=U` are fine (read-only).
Task: (1) resolve every marker preserving BOTH sides — your side = typed cap results/replay, workspace receipts, partial progress, exit 78,
adapter [fidelity] TOML; wave side = providerrebind (rebind RPC + daemon.caching), casstream (CAS shapes), toolshape (truncation footer,
effects). (2) NEVER hand-merge a golden: regenerate every affected fixture/JSONL golden through the repo's tooling (UPDATE_FIXTURES=1 /
the bless path) and review each changed line for exactly the expected additive changes. (3) Re-pin the instruct-pipe byte count in
crates/haider-daemon/src/permissions_core_tests.rs to the REAL merged value (say old -> new). (4) The handshake feature pin in
crates/haider-daemon/src/connection_tests.rs: run `cargo test -p haider-daemon --lib welcome_features_pin_served_management_families`,
read the `left:` value, set the pin to it. (5) Recount test-baseline.txt with the test-count tool. (6) Full gate under the ENV LAW:
`cargo test -q --workspace --no-fail-fast` and `cargo clippy --workspace --tests -- -D warnings` (verbatim). Leave the resolved tree in
place and STOP — the orchestrator commits the merge. Report per-file resolution, golden review, full-gate totals, clippy exit, the
baseline, and the MANDATORY `VERIFIER: findings=<n> real=<n> noise=<n> — …`. LAST line SHIP or NO_SHIP.
