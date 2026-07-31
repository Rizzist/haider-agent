# W9b — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w9-headless`, lane commit 62d69be, review
fixes e4170cf. Implementer: codex lane (gpt-5.6 xhigh) per
docs/briefs/W9b-headless-run-brief.md.

## What shipped (lane)

Daemon-backed `haider run` — the in-process SQLite/`HarnessActor`
authority is DELETED. Reusable headless transaction in haider-client
(Headless hello → create → Control attach barrier → submit → cursor
stream reduction → terminal-only completion); wall-clock timeout with
one durable cancel and bounded terminal grace; permission asks answered
by TYPED `RejectOnce` (server-enumerated, never index/label) with the
denial exposed; `SessionPermissionOverridesV1` (allow_writes/allow_exec)
additive on SessionCreate + durable metadata, in the create digest,
applied after registry defaults, journaling ordinary policy `Allow`
(never forged user-typed provenance), feature-gated by
`session_permission_overrides_v1`; print/json/jsonl output laws + the
full exit-code table. Ledger 1231 → 1262.

## Host review of record — the socket suites the sandbox could not run

The codex sandbox cannot bind UDS, so the 19-test headless suite and the
real-daemon CLI tests ran FIRST on this host, under review. Four tests
were scheduler-flaky and two CLI tests failed outright. Root-causing
them surfaced THREE REAL CLIENT DEFECTS (all fixed in e4170cf):

1. **Answers-then-closes daemons lost their final answers.** A writer
   EPIPE (typically the heartbeat ping racing a peer close) called
   `Shared::fail`, clearing pending waiters and aborting the reader
   while undelivered response/terminal frames sat in the socket. Fix:
   writer failure is DEFERRED; the reader drains to EOF first; the pong
   deadline bounds the half-open case.
2. **A resolved response lost ties to the disconnect signal.** The
   submit wait `select!` was unbiased — with both ready, the disconnect
   arm could win and discard the already-resolved response, spending a
   reconnect (against a daemon that just answered). Fix: `biased;`
   response-first.
3. **Loss recovery could abandon deliverable frames.** The lost-events
   check ran between recvs; mid-burst it reconnected while applicable
   frames sat queued. Fix: recovery is gated on a DRAINED channel.

Test-suite verdicts from the same review:
- External channel saturation is UNREACHABLE by design (the runner's
  forwarding backpressures; 2000-event blasts still apply cleanly) — the
  saturation scenario was a scheduler lottery. Rewritten as
  `lagged_pressure_recovers_every_durable_sequence`: the daemon's own
  `Lagged` frame drives the same cursor recovery deterministically.
- The one-cancel assertion treated the runner's legal clean close as a
  panic; a close-tolerant `try_next` pins the real law.
- The missing-credential CLI test inherited the developer's REAL
  profile (real credentials defeated the law) — now hermetic; the CLI
  gained the actionable remedy line the test always intended.
- Pre-acceptance timeouts skipped the json contract — the CLI now emits
  the `haider.run.v1` object for EVERY outcome (null ids when no run was
  accepted).

Post-fix: headless suite 19/19 × 15 consecutive runs; CLI 29/29.

## Mutations (reviewer-chosen, EXECUTED post-commit at e4170cf)

| # | Mutation | Result |
|---|---|---|
| M1 | permission overrides never apply | KILLED (`session_permission_overrides_replace_only_write_and_exec_ask_defaults`) |
| M2 | permission answer picks index 0 (ignores typed decision) | KILLED (2 tests) |
| M3 | timeout maps to 130 (both maps) | KILLED (exit-table law) |

Plus the implementer's mutation table (docs/briefs/
W9b-headless-run-mutation-notes.md) — spot-verified through the three
fixes above, which exercised exactly those seams at runtime.

## Gate

gate37: full per-crate gate GREEN (fail=0) — client 37, cli 33, daemon 207, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (merges with W9a as v0.0.37). · ledger 1262.
