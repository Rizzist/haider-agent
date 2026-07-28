# TUI6 review round 5 — SHIP_WITH_FIXES (one focused fix; all three residuals ledgerable)

Reviewer: gpt-5.6 (codex), frozen 4a44106, scope 65d9353..4a44106 (TUI6.4).

The r4 FIFO cross-attempt exploit is CLOSED (identity is per-request CommandContext + RPC request_id waiter, not FIFO; distinct-card correlation HOLDS; retired replies all silent). P2 CLOSED. Residuals all adjudicated SHIP-WITH-LEDGER: P3 local Encode→Failed{None} (fails before waiter insert + before wire — no vault reference can exist, deadline recovers); P4 no dedicated link-routing seam pin (transitive coverage); P4 inert retired_logins orphan (unique command ids + duplicate-response discard = cannot bind; accumulates until disconnect — inert debt). Mutation audit 3/3 killed+restored. 868→870, no deletions.

REQUIRED FIX (TUI6.5 — the real close): attempt identity is CARD-scoped (minted at card open app.rs:2550, reused every retry) but must be STAGE-ISSUANCE-scoped. Timeout clears the driver binding but neither changes the open card's attempt nor cancels the outstanding stage waiter (live.rs:1166); retype makes the same attempt live again; a late reply from the timed-out stage passes both gates → probe minted LoginApi{ vault_reference: "OLD-TIMED-OUT-VAULT-REFERENCE", attempt: 1 }. Sibling: live_pass applies an inbound Staged BEFORE expiring the deadline (runtime.rs:2157) — a late stage mints, then expiry retires internal state but not the already-returned command.
Fix: (1) every Stage issuance gets a FRESH identity independent of card lifetime; (2) permanently invalidate the previous issuance before retype/retry; (3) reject/expire a stage before it can mint once its deadline elapsed (fix the apply-before-expire ordering in live_pass); (4) pin both timeout→retry→late-old-stage AND at-deadline-stage, asserting no stale vault reference leaves live_pass.

Ledger these three (docs/OPTIMIZATIONS.md, in the TUI6.5 commit): the P3 local-encode 30s-recovery residual, the P4 link-routing seam-pin gap, the P4 retired_logins orphan accumulation-until-disconnect.

Round 4’s exact FIFO cross-attempt exploit is fixed, and P2 closes. However, the new identity remains card-scoped rather than stage-issuance-scoped, producing a new P1 wrong-credential path. Merge and v0.0.13 require one focused fix round.

## Closure rulings

- **P1 — PARTIAL / must-fix.** The `[retired N, live N+1]` probe passes: an intervening `Failed { None }` cannot shift correlation, old `Staged(N)` emits nothing, and `OLD-CANCELLED-VAULT-REFERENCE` never appears in `LoginApi`.
- **P2 — CLOSED.** A retired `Failed` leaves the card and flash untouched. Retired `LoggedIn`, `Staged`, and `StageFailed` siblings are also silent: no visible paint, flash, or credential command.

## New-mechanism attack

Distinct attempts are correctly correlated:

1. `Stage` captures its attempt in a per-request `CommandContext`.
2. The RPC envelope’s `request_id` selects that request’s one-shot waiter.
3. The waiter synthesizes `Staged { attempt }` or `StageFailed { attempt }`.

Therefore, responses for distinct attempts may interleave without swapping identities. This is not FIFO association. The client removes the waiter before delivering the first response, so a duplicate response with the same request ID is discarded.

The defect is retry identity reuse:

- The attempt ID is minted when the card opens and reused for every retry ([app.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2550)).
- Timeout clears the driver binding but neither changes the open card’s attempt nor cancels the outstanding stage waiter ([live.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1166)).
- Retyping makes that same attempt live again.
- A late response from the timed-out stage then passes both gates.

The temporary probe reproduced:

```text
LoginApi {
  vault_reference: "OLD-TIMED-OUT-VAULT-REFERENCE",
  attempt: 1,
  ...
}
```

There is also a same-pass boundary: `live_pass` applies an inbound `Staged` before expiring the deadline ([runtime.rs](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2157)). A late stage can therefore mint `LoginApi`, after which expiry retires internal state but does not remove the already-returned command.

Required fix:

- Give every `Stage` issuance a fresh identity, independent of the card lifetime.
- Permanently invalidate the previous issuance before retype/retry.
- Reject/expire a stage before it can mint when its deadline has elapsed.
- Pin both timeout→retry→late-old-stage and at-deadline-stage cases, asserting no stale vault reference leaves `live_pass`.

## Residual adjudication

| Residual | Ruling | Reasoning |
|---|---|---|
| P3 local `ClientError::Encode` yields `Failed { None }` | **SHIP-with-ledger** | Encoding fails before waiter insertion and before anything reaches the wire. No vault reference can exist or cross-bind. The already-armed 30-second wakeup recovers the card. Identity-tagging it would improve immediate recovery but is not credential-critical. |
| P4 no dedicated link-level routing pin | **SHIP-with-ledger** | Coverage gap, not a demonstrated defect. Driver probes and request-ID client tests cover the mechanism transitively, but `VaultStage/Error → attempt-tagged reply` deserves a direct seam test. |
| P4 inert `retired_logins` orphan | **SHIP-with-ledger** | A retryable response followed by retirement can leave an ID whose only response was already consumed. It cannot suppress or bind another credential because command IDs are unique and duplicate responses are discarded. Repeated shapes can accumulate until disconnect, so it is not strictly cardinality-bounded, but remains inert P4 debt. |

## Mutation audit

| Mutation | Confirmed failure |
|---|---|
| Weaken identity comparison | Round 4 probe minted `OLD-CANCELLED-VAULT-REFERENCE`. |
| Invert/remove the non-live drop | Same cancelled reference minted. |
| Disable retired-failure suppression | `· provider_rejected — old attempt failed` flash returned. |

Each mutation was restored independently. Final source hashes match the frozen tree.

## Regression and gate

- Tests: **868 → 870**, exactly two additions; no deleted test functions, attributes, or assertions.
- The two 6.3b tests were re-scoped from positional `Failed(None)` handling to identity-tagged `StageFailed`; immediate recovery and next-attempt liveness assertions remain.
- `xtask test-count`: **870/870**.
- Login suite: **21/21**; TUI6 suite: **49/49**.
- Clippy `--workspace --all-targets -- -D warnings`: pass.
- Formatting: pass.
- Release CLI/daemon build: pass.
- Workspace no-fail-fast inspection: zero `could not compile`; 8 targets/88 tests failed solely in UDS/process-backed suites, with 82 explicit `PermissionDenied`/`Operation not permitted` markers.
- Ladder: **14/14 demo rows pass**. Both live rows failed before alt-screen/daemon readiness under the same UDS prohibition; supplied orchestrator evidence is 16/16 at `4a44106`.
- Synthetic merge against `d9d66b4`: no conflict markers; diff checks pass.
- Final state: HEAD `4a441069…`; porcelain, tracked/index diff, and stash empty.

## Login-modality law

| Component | Ruling |
|---|---|
| Secret hygiene | **HOLDS** |
| Distinct-card correlation | **HOLDS** |
| Stage-issuance correlation across timeout/retry | **VIOLATED** |
| Liveness | **HOLDS** — identity-tagged wire errors recover immediately; uncorrelated/local failures remain deadline-bounded |
| Composite login-modality law | **VIOLATED** until stage issuance identity and deadline ordering are fixed |

## New findings

| Tier | Finding |
|---|---|
| P0 | None found. |
| P1 | Card-scoped attempt identity is reused after timeout; a late old stage can emit/commit the previous credential during a retry. The apply-before-expire ordering provides a sibling stale-command path. |
| P2 | None found. |
| P3 | None beyond the ledgered local-encode residual. |
| P4 | No new defects beyond the two ledgered coverage/state-lifetime residuals. |

VERDICT: SHIP_WITH_FIXES
