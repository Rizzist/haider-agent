# Patch brief W2a/C4a.1 — permission-boundary fixes (6 findings)

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. FULL findings with
file:line in docs/briefs/C4a-review-1-NO_SHIP.md — READ IT FIRST. Fix ALL:
1. (P1) Authorization binds to the INTENT (effect_id + args_digest pair), never transferable:
   verdicts keyed by both; a different digest under the same id = Lifecycle error + re-auth.
2. (P1) Remove into_journal entirely (the bypass class returns via ownership — the sink stays
   inside the broker; provide a read-only snapshot/query API for tests instead).
3. (P1) Generation-stamp effect/menu/permission ids like evt-ids (session+generation+start_ms+
   counter — take generation as a broker constructor param; the actor supplies it). Restart
   test with frozen clock.
4. (P1) Workspace boundary: broker/filesystem take a workspace ROOT; every path canonicalized
   (parent-dir canonicalize for to-be-created files) and MUST be under the root — reject
   traversal/symlink escape with a typed error + test. Digest binding: canonicalize the PATH
   (post-boundary-resolution) into the digest input so lexically-different aliases of the same
   file share a digest and different files never do.
5. (P1) Atomic preimage+apply: read-verify-write under one exclusive open (read the file once,
   verify preimage, write derived content to temp + rename over — no TOCTOU window); ledger
   entry appended from the SAME content that was written (record bytes-hash), and appended
   BEFORE outcome with a test proving a failed outcome append still leaves the ledger entry.
6. (P2) Malformed menu answers fail CLOSED: unknown key AND out-of-range index → typed error,
   re-ask; never index-fallback to an unintended option.
Gate: cargo test -p haider-tools, workspace clippy -D warnings, fmt, xtask test-count --update.
Leave uncommitted.
