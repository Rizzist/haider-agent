# Lane ceilingdecl — CONTINUATION 3: the wave moved again (journalview landed); the merge is STARTED for you (v0.0.970)
Your previous merge-forward (onto 38359fd3) is COMMITTED (c5126f66). The landing chain then had to merge onto the NEW head 368f093c —
journalview landed: assistant-text/reasoning narrative events, `provider_rounds`, scoped `context_compaction` announcements, all of which
touch the same JSONL goldens and crates/haider-core/src/actor.rs as your cap/receipt/end_reason work. The orchestrator has run
`git merge --no-commit origin/wave-970` in this worktree: MERGE_HEAD is set, markers are in the WORKING TREE. You need NO git metadata
access: edit files only (no git merge/commit/checkout/reset).
Task: (1) resolve crates/haider-core/src/actor.rs preserving BOTH sides (journalview's narrative capture points + your cap check that
precedes the provider refresh — keep that precedence and its regression). (2) NEVER hand-merge a golden: regenerate
crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl and crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_{text,tool}_turn.jsonl
through the repo's tooling and review every changed line for exactly the expected additive changes (journalview's narrative/correlation
events + your cap/receipt fields). (3) test-baseline.txt: recount with the test-count tool. (4) Re-check the instruct-pipe pin and the
handshake pin from the tests' own output. (5) Full gate under the ENV LAW: `cargo test -q --workspace --no-fail-fast` and
`cargo clippy --workspace --tests -- -D warnings` (verbatim). Leave the resolved tree and STOP — the orchestrator commits. Report per-file
resolution, golden review, full-gate totals, clippy exit, the baseline, and the MANDATORY `VERIFIER: findings=<n> real=<n> noise=<n> — …`.
LAST line SHIP or NO_SHIP.
