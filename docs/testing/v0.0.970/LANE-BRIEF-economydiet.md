# Lane economydiet — CONTINUATION: the merge onto wave-970 (9270f402) is STARTED for you; resolve it in the working tree (v0.0.970)
Your implementation is COMMITTED (55982a52). The orchestrator ran `git merge --no-commit origin/wave-970` in this worktree: MERGE_HEAD is
set and FOUR files still carry <<<<<<< markers in the WORKING TREE (the registry walk and test-baseline.txt are already resolved).
You need NO git metadata access: edit files only (no git merge/commit/checkout/reset; read-only git commands are fine).
Conflicts to resolve: crates/haider-daemon/src/permissions_core_tests.rs (the instruct-pipe byte pin — keep both sides' intent and re-pin
to the REAL merged value, say old -> new), and the three JSONL goldens crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl,
crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_{text,tool}_turn.jsonl — NEVER hand-merge a golden: regenerate them through the
repo's tooling and review every changed line for exactly the expected additive changes (the wave side since your base: customprov,
ceilingdecl cap/receipt fields, journalview narrative/provider_rounds; your side: the slimmed envelope + tiered tool exposure). Then:
handshake pin from the test's own `left:` output if it drifted; test-baseline.txt recount with the test-count tool (currently 4991 after
the orchestrator's recount — re-verify). Full gate under the ENV LAW: `cargo test -q --workspace --no-fail-fast` and
`cargo clippy --workspace --tests -- -D warnings` (verbatim). Re-measure the AHRB fixed overhead on the MERGED tree the same way you did
(report before/after; the merged number must still be at least half the 14,222 baseline). Leave the resolved tree and STOP — the
orchestrator commits. Report per-file resolution, golden review, the merged overhead numbers, full-gate totals, clippy exit, the baseline,
and the MANDATORY `VERIFIER: findings=<n> real=<n> noise=<n> — …`. LAST line SHIP or NO_SHIP.
