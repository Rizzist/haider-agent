# Lane monitorcore — CONTINUATION 4: fix the post-merge compile error against replyarena2 (v0.0.970)
Your merge onto wave-970 is committed (two parents). The landing chain then merged this branch forward onto the NEW wave head
(replyarena2 landed: canonical reply arena — `ReplyText`/`RawPayload` types replace String payloads on reply/record paths) and
`cargo test -p haider-daemon` no longer compiles: see docs/testing/v0.0.970/monitorcore-landing-daemon-compile-error.txt (E0308
mismatched types in the daemon lib test target). The merge-forward commit is at HEAD of this worktree; the tree is clean.
Task: fix the semantic conflict the right way (adapt the monitor code/tests to the reply-arena types; do not weaken either side; no
`.to_string()` band-aids where a typed value is expected — use the arena's constructors as the rest of the daemon does). Then, with the
ENV LAW, run `cargo test -p haider-tools -p haider-rpc -p haider-store -p haider-platform -p haider-daemon -p haider-daemond` and
`cargo clippy --workspace --tests -- -D warnings`, update test-baseline.txt with the repo's test-count tool. You cannot commit (worktree
git dir is outside your sandbox) — leave the fix in the working tree and STOP. Report the fix, suite totals, clippy exit, baseline.
LAST line SHIP or NO_SHIP.
