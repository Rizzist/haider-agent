# Patch brief W2a/C4a.2 — round-3: fd-anchored fs + cancellation-shielded ledger

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. Findings w/ file:line
in docs/briefs/C4a-review-2-NO_SHIP.md. Resolutions:
1. (P1 boundary race) FD-ANCHORED path discipline: hold an open dirfd for the workspace root;
   resolve every target RELATIVE to it using openat-style APIs (rustix crate — add to
   workspace deps — openat2 unavailable on macOS; use O_NOFOLLOW per-component walk or
   rustix::fs::openat with NOFOLLOW on the final component + reject `..` components POST-
   canonicalization of the RELATIVE path string). A symlink swapped after authorization gets
   ENOENT/ELOOP → typed error, never an out-of-root access. Test: authorize, swap a path
   component to a symlink pointing outside root, apply → typed refusal.
2. (P1 preimage atomicity) Same-fd read-verify-write: open target once (O_NOFOLLOW), read via
   the fd, verify preimage, write derived content to a temp IN THE SAME DIRECTORY via dirfd,
   rename-over via renameat anchored to the dirfd. CONTRACT the residual in the module header:
   the broker guarantees serialization among BROKER-MEDIATED writes (Haider's workspace
   doctrine — AI writes all pass here); concurrent NON-broker writers are outside the
   guarantee and belong to external-edit detection (§9.2 doctrine) — reviewed against THIS
   stated contract, mirroring the supervision harness's residual-window precedent.
3. (P1 cancel/ledger) The apply+ledger pair is a CRITICAL SECTION: once the rename lands, the
   ledger append MUST complete regardless of cancellation (shield it — no await points
   between rename and ledger append that can observe cancel; if the ledger append itself
   fails, the outcome is Failed-with-ledger-error, never silent). Test: cancel racing the
   apply window → assert either (no write) or (write AND ledger entry) — never write-only.
Gate: cargo test -p haider-tools, workspace clippy -D warnings, fmt, xtask test-count --update.
Leave uncommitted.
