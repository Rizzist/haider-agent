# Lane turnbudget — the per-turn request cap must not silently kill long work (v0.0.970, gpt-5.6 xhigh)
Worktree lane-970-turnbudget (from origin/wave-970). EVIDENCE (/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log):
17 of 20 benchmark runs died on DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN = 32 (crates/haider-core/src/actor.rs:338, enforced at
actor.rs:3376) with the workspace untouched — exit 70, work discarded. opencode has NO default cap (maxSteps = agent.steps ?? Infinity)
and its solved runs used up to 53 rounds. CLAIM-AUDIT first.
Deliver: 1. Raise the hard ceiling to at least 64 (justify the number). 2. Better: make 32 a SOFT tranche — at the soft bound the agent is
told, in a typed model-readable note, that it has used its tranche and must either finish or checkpoint, and the run continues to a
separate hard ceiling. 3. Hitting either bound must be a visible, RESUMABLE state, not a discarded turn: the partial work, the tool
history and a continuation handle survive so the next turn resumes rather than restarts. 4. Surface the budget in the TUI/journal (used vs
tranche vs hard cap) so a user can see it coming. 5. Configurable per run and per agent.
Tests: soft-bound note is emitted exactly once and is model-readable; hard bound terminates with a named, typed cause; resume-after-bound
restores tool history and continues; the counter still ignores transport retries (provider_attempt == 0 semantics); existing loop-limit
pins updated with rationale. `cargo test --workspace`, clippy -D warnings, ENV LAW, test-count update.
docs/testing/v0.0.970/turnbudget.md. Commit on the lane branch, no trailer, no push. LAST line SHIP or NO_SHIP.
