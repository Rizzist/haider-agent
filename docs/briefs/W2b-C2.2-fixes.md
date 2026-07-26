# Patch brief W2b/C2.2 — round-3 fixes (6 P1 + 1 P2)

Worktree /Users/rizzist/haider-run/haider-c2, branch w2b-c2. Findings in
docs/briefs/C2-review-2-NO_SHIP.md. The r1 structures are verified — do NOT restructure
sealed provenance, fd anchoring, KILL escalation, turn-loop shape, or MenuClosed.

1. Cancel-vs-CAS window: re-check cancellation AFTER `cas.put_file` resolves (success or
   error); if cancel was requested at any point, the outcome is Cancelled (sticky rule
   extends through ingestion). Test: cancel DURING a blocked put_file (gate the CAS with
   a barrier double) on both the success and failure arms.
2. FileCas::put_file single-pass integrity: stream the source through the hasher WHILE
   writing the temp copy; publish under the digest of the bytes actually written (hash
   what you copied, not what you read earlier). Concurrent source mutation then yields a
   self-consistent object, never a corrupt digest. Test: mutate the source between
   phases via a wrapper reader and assert the published object matches its digest.
3. Recycled-PGID: reorder — perform the group sweep while the leader is a ZOMBIE
   (after child exit observed via try-wait/exit event, BEFORE reaping wait()): a zombie
   leader keeps the pgid allocated, so killpg cannot hit a recycled group. Document the
   ordering invariant in the sweep fn header. Test ordering via the registry/sweep hooks.
4. Turn-loop ceiling: max provider requests per turn (config, default 32). Exceeding →
   typed Errored outcome (LoopLimit) with the count in the error; test at a small
   configured ceiling.
5. Submit flood: cap the deferred command queue (config, default 64) — beyond it,
   Submit is rejected with a typed busy error at the API boundary; and restructure the
   select so provider progress cannot be starved indefinitely (poll provider each
   servicing round; cancel stays prompt). Test: flood submits while a stream runs →
   stream still completes, queue bounded, rejects surfaced.
6. env-view classifier: add PASSWD/PWD/PASSPHRASE substrings + known names (PGPASSWORD,
   MYSQL_PWD, AWS_SECRET_ACCESS_KEY, GITHUB_TOKEN, NPM_TOKEN, LD_PRELOAD? no — secrets
   only) and case-insensitive matching; tests for PGPASSWORD and MYSQL_PWD redacted,
   PATH/HOME still shown.
7. P2 peak-memory assertion: expose a test-observable transcript high-water (cfg(test)
   counter or sink instrumentation) and assert during the paused-clock flood that
   in-memory payload never exceeds cap + max-chunk while the spill file grows.

Gate: cargo test --workspace, clippy -D warnings (all targets), fmt --all --check,
xtask test-count --update, git diff --check. Leave changes uncommitted.
