# Lane runperm — CONTINUATION: fix the run.rs parser semantic conflict with turnbudget, full gate (v0.0.970)
Your implementation + merge-forward are COMMITTED on lane-970-runperm (HEAD = merge 2ef810ff onto wave b9c2a047). The landing chain's full
workspace gate FAILS on ONE test, seen in 5 binaries: `run::tests::resume_parser_accepts_handle_and_budget_without_prompt` panics at
crates/haider-cli/src/run.rs:2209 — see docs/testing/v0.0.970/runperm-landing-workspace-failure.txt for the panic text. That test belongs
to turnbudget (landed after your base): `haider run --resume <handle>` with a budget flag must parse WITHOUT a prompt. Your lane changed
the same parser (default allow_writes/allow_exec, `--read-only`, autonomous Ask->Allow). This is a semantic conflict, not a golden.
Task: (1) `git fetch origin wave-970 && git merge --no-commit origin/wave-970` first — toolrepair may have landed since (aliases,
invalid_tool_call); resolve preserving both sides (expect the usual trio: provider_request_no_budget.json regenerate via tooling,
permissions_core_tests.rs byte pin re-pinned to the real value, test-baseline.txt recount). (2) Fix the parser so BOTH contracts hold: a
resume handle + budget parses without a prompt (turnbudget), and the flagless default is write+exec-capable with `--read-only` as the
opt-out (runperm). Do not weaken either test; if the two flags genuinely conflict, say which wins and why, in the evidence doc. (3) Full
gate under the ENV LAW: `cargo test -q --workspace --no-fail-fast` and `cargo clippy --workspace --tests -- -D warnings` (verbatim), plus
the test-count update. You cannot commit (worktree git dir outside your sandbox) — leave the tree resolved and STOP; the orchestrator
records the merge. Report the fix, per-file merge resolution, full-gate totals, clippy exit, the baseline, and the MANDATORY line
`VERIFIER: findings=<n> real=<n> noise=<n> — …`. LAST line SHIP or NO_SHIP.
