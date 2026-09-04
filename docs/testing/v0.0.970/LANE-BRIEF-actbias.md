# Lane actbias — the agent must move from reading to writing (v0.0.970, gpt-5.6 xhigh)
Worktree lane-970-actbias (from origin/wave-970). EVIDENCE (/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log): in a
3-pair transcript audit haider made ZERO file writes and ZERO test runs, spending 31-54 filesystem calls re-reading and re-searching the
same files while opencode solved the same tasks with probe -> single edit -> probe. Cited causes: (a) haider's immutable policy contains
NO ordinary coding sequence (no inspect -> edit -> verify contract) and no worked editing example, while opencode's default prompt
(~8.33 KB) carries both plus explicit instruction to act (OPENCODE packages/opencode/src/session/prompt/default.txt:45-69); (b) haider
empties native tool descriptions and moves semantics into a system manual (HAIDER crates/haider-daemon/src/worker.rs:13109-13143,
13230-13263), so the schema a weak model sees is bare. CLAIM-AUDIT first.
Deliver: 1. Add a short, explicit action contract to the system prompt: inspect only until the target is known, then make the edit, then
verify (build/test), and do not remain in planning on an implementation request. Keep it terse — this is the prompt every turn pays for.
2. Restore concise native descriptions on the mutation and search tools (name, what it does, when to use it) instead of relying on the
manual alone. 3. One worked example of the search -> edit -> verify sequence in the prompt. 4. Measure the prompt-byte delta and report it;
the instruct pipe is pinned at 11,246 bytes by a release test — update the pin deliberately and say why.
Tests: prompt-content pins (contract present, example present, byte-size pin updated with rationale), tool-schema description pins,
existing prompt/golden tests green. `cargo test --workspace`, clippy -D warnings, ENV LAW, test-count update.
docs/testing/v0.0.970/actbias.md. Commit on the lane branch, no trailer, no push. LAST line SHIP or NO_SHIP.
