# W3c3 review round 3 — SHIP (closing round)

Reviewer: gpt-5.6 (codex), frozen b9b68e7, scope a09647f..b9b68e7 (the four r2 fixes). All four CLOSED; mutation revert executed externally, both pins killed, restored byte-identical; the seam-test-over-socket-flood replacement ruled STRONGER (client channel caps at 256 < the 512 barrier — a socket flood exercises a different loss class). Strict-gap HOLDS end-to-end. NEW FINDINGS: none at any tier. Live ladder rows environmental in the reviewer sandbox; 16/16 on the orchestrator machine (multiple runs).

The W3c3 arc: dual r1 NO_SHIP (codex 6 P1 + design 3 D1, double-attach found independently by both) → W3c3.1 (laws + attach authority + live_pass) → W3c3.2 (Fable review-of-Opus + completion; found the AttachFailed ping-pong P1) → r2 NO_SHIP (4 enumerated) → W3c3.3 → r3 SHIP.

## Closure rulings

1. **NF-1 — CLOSED.** At cap 512, reply 513 defers exactly 513 losses, clears, and subsequent replies re-accumulate; repeated overflows coalesce with saturating arithmetic. Final settlement—including an empty failed/disconnected attach batch—decrements the barrier, flushes held replies, then publishes loss ([link.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:156)). Disconnect correctly discards deferred loss because reconnect preserves the working set and reattaches from applied cursors. The hidden public seam is sharp but acceptable under the existing `CommandContext` precedent and has no production callers beyond `run_link`.

2. **NF-2 — CLOSED.** Local attach encoding failure now emits session-correlated, permanent `AttachFailed` ([link.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:367)). The reducer releases both latch and LRU slot, marks the session cold, and deselects only when active ([live.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:882)); background failures prune cleanly without disturbing the active surface. Other `Failed(None)` producers are List, Detach, Stage, and protocol notices: none owns an attach latch, and Stage retains its accepted bounded login-timeout recovery. `PendingResponse::wait` currently produces only success or `Disconnected`; its generic non-disconnect branch is unreachable today.

3. **NF-3 — CLOSED.** `PeerClosed | Io(_)` honestly represents the documented first-reason-wins transport race. `Protocol`, `Fatal`, `Closed`, and `PongTimeout` remain rejected by the test’s match ([w3c3_ordered_send_tests.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-client/tests/w3c3_ordered_send_tests.rs:248)).

4. **NF-4 — CLOSED.** The charter now accurately names both audit mechanisms: the exhaustive `AppRequest` match and the per-surface `w3c31_r2_tests` gates ([app.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:874)). Both exist and perform the claimed roles.

## Attack and regression results

- Exact inline-publish mutation executed in external scratch. Both pins failed:
  - behavioral seam: “nothing may publish while the attach is outstanding”
  - source guard: “the overflow arm defers; it must not publish mid-barrier”
- Scratch mutation was restored byte-identically.
- The seam test is stronger for barrier overflow: the client event channel is capped at 256, below the 512 barrier cap ([client.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-client/src/client.rs:42)), so socket blasting first exercises client-channel loss. The seam deterministically reaches the precise boundary; the source guard pins post-flush placement.
- Tests: 810→812; no test file, test function, or test attribute deleted.
- `cargo test --workspace`: passed, no `FAILED`.
- Clippy `--workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `xtask check`: 812/812; zero files above the soft cap.
- Release build: passed.
- Ladder: demo 14/14. Live PTY rows marked environmental after the first row could not enter alt-screen or create its daemon in this sandbox; daemon-backed workspace UDS suites passed.
- Final state: exact HEAD `b9b68e77b04cf1136fb328c0cab09e81def5bc64`; porcelain, worktree diff, index diff, diff-check, and stash all empty.

**Law:** strict-gap now **HOLDS end-to-end**—deferred loss cannot precede attachment installation, and disconnect recovery resumes from the reducer’s applied cursor.

**New findings:** P1 none found; P2 none found; P3 none found.

VERDICT: SHIP
