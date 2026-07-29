# W5b review — round 2 (fix-round confirm of W5b.1)

- Frozen SHA: `4cd8542` (branch `w5-b`), diff `2d5c891..4cd8542`.
- Method: dual review of record — gpt-5.6 (xhigh, 2 verification subagents) correctness lens + Fable independent confirmation of the blocking findings.
- Baseline: 985/985; my socket-capable full-workspace gate green (0 fail / 0 compile-err / 0 panic); fmt + whitespace clean.

## Per-P1 status (round-1 findings)

1. Barrier ownership — **STILL-OPEN** (deeper). Direct broker/coordinator tasks are now owned + joined, but refresh persistence escapes into an abort-only account actor + blocking pool.
2. Late-failure fence — **CLOSED**. `expire_oauth_refresh` (accounts.rs:820) rechecks descriptor identity + generation/issuer/audience/resource/subject before mutation.
3. Durable rotation-failure expiration — **STILL-OPEN** (deeper). Fail-closed persist exists, but generation N+1 can publish before the descriptor restoration/fail-closed outcome is durable.
4. Issuer/audience/resource binding — **CLOSED**. `refresh_bundle_from_response` (oauth.rs:2429) rejects registration drift + returned-token binding mismatch; issuer bound by the immutable registration endpoint. (W5b.2 prerequisite satisfied.)

## Blocking findings (VERDICT: NO_SHIP)

- **[P1] barrier not transitively closed** — `runtime.rs:551`. On forced teardown the broker and coordinator get `abort_and_join()` (runtime.rs:541/549), but the account actor path only sets `forced=true` and falls through to `drain_sender.send_replace` (runtime.rs:565) with NO join of its in-flight work. Refresh enters `spawn_blocking` vault persistence (accounts.rs:732); a `spawn_blocking` task cannot be aborted, so the cleartext-token-bearing vault mutation outlives `Stopped` and can race a successor daemon. Fix: forced teardown must be transitively closed — the actor's outstanding blocking persistence must be joined (bounded) and/or the vault write fenced by daemon generation/instance so a late completion is inert against a successor; cleartext token bytes zeroized on teardown. (Fable-confirmed at runtime.rs:551-565.)

- **[P1] publish-before-commit on refresh admission** — `oauth.rs:1905` → `oauth.rs:1943`. `resolve()` admits an OAuth resolve when `has_active_flight` is true even while the descriptor is mid-rotation; `resolve_oauth` returns `bundle.access_token_handle()` (oauth.rs:1943) as soon as the vault bundle is unexpired. If the rotation's descriptor restoration then fails and fail-closes by deleting the bundle, generation N+1's access token has already escaped to a turn. Violates INV-1 persist-before-publish. Fix: a rotated bundle must not be resolvable/returnable until its descriptor restoration/fail-closed outcome is durably committed. (Fable-confirmed at oauth.rs:1904-1944.)

## Non-blocking (fix in the same round)

- **[P2] JoinSet errors discarded** — `oauth.rs:82`. `OwnedTaskSet` drains `try_join_next`/`join_next` without inspecting `JoinError`. A refresh worker panic before `flight.finish()` leaves the flight registered → all same-key refreshers block indefinitely, and graceful shutdown still reports completion. Fix: surface panics and poison/finish the orphaned flight so waiters fail closed.

- **[P2] vacuous secret-sweep test** — `oauth_rpc_tests.rs:784`. The claimed TUI sweep formats DTOs (`{descriptor:?}...`) instead of rendering the actual TUI; a real renderer leak would pass. Live-HTML assertions omit dynamic nonce + callback-path values. Fix: render the real TUI surface + assert nonce and callback-path absence; then confirm the mutation kills.

- **[P2] vacuous resource-binding coverage** — `oauth_tests.rs:217`. Outbound refresh coverage asserts the audience form field but not resource; deleting the resource binding leaves the suite green. Fix: assert the resource form binding; confirm the deletion mutation kills. (This pin's "Verified by revert" in W5b.1 was false-confident — reviewer-re-execution law vindicated.)

## Required for W5b.1b SHIP

Fix both P1 + all three P2. Re-run the FULL mutation audit — the two now-corrected vacuous tests MUST kill their mutations, plus new mutations for the two P1 fixes (blocking-persistence join/fence bypass; publish-before-commit bypass). Full gate in a socket-capable env. Re-review round 3 focuses the two P1 closures + the corrected audit.
