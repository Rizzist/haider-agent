# W5f-2 + W5f-3 — review of record #1 — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-f2` @ `729cb99`.
Authority: the owner's end-state target ("consume oauth … create a
session, choose model … and session turns work") + the two named gate
flakes from the W5e-3 ledger.

## W5f-2 — identity follows daemon truth; output cap

Two live-turn killers found by READING the create path before probing:

1. **The composer identity never reconciled with daemon reality.** The
   launcher created sessions on whatever the demo seeds left in
   `IdentityLine` — a provider with no account, instant `✗ ERRORED` (the
   owner's screenshot #50). Now: when live account/provider snapshots
   apply, an UNPINNED identity adopts the active account's provider +
   that provider's own declared default model (never an invented slug).
   `/model`, `/provider`, and clicking an account PIN the identity;
   clicking also finally ADOPTS the account into the composer line —
   choosing an account is choosing the session identity.
2. **`session.create` sent the context window as the output cap.**
   `metadata.max_tokens` reaches the providers as `max_output_tokens` /
   `max_tokens` — the per-request OUTPUT budget. 200k gets an immediate
   Anthropic 400. `SESSION_OUTPUT_CAP` (30k, bounded by a smaller
   declared window, floored at 1) rides instead.

## W5f-3 — the two named flakes

- `haider-client::fatal_protocol_error_frame_fails_pending_requests`:
  the fixture's own keepalive (ping/pong deadlines) could fire under
  full-gate CPU starvation before the fake daemon's fatal frame routed,
  resolving the request to a different disconnect variant. The property
  under test is frame ROUTING, not keepalive timing — the fixture now
  runs with 120s keepalive room.
- `haider-daemond` support `DEADLINE` 10s → 60s (three sightings, all
  under full-gate contention, never isolated). Passing runs never wait;
  only real failures pay the longer bound.

## Mutations (executed post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | driver drops both bootstrap calls | KILLED — 1 test |
| M2 | bootstrap ignores `identity_pinned` | KILLED — 2 tests |
| M3 | account click adopts without pinning | KILLED — 1 test |
| M4 | create passes the raw context window | KILLED — 1 test |

## Gate

haider-tui full suite green (47 binaries); haider-client green; clippy
`-D warnings` clean; ledger 1097 → 1101. Full per-crate gate:
`gate13.out`.

## Honest residuals

- The bootstrap keys on the FIRST selected account row (daemon truth
  orders them); with accounts on several providers the identity follows
  whichever the daemon lists selected first — `/provider` or a click
  overrides in one action. Fine at this maturity.
- `context_window` itself still comes from demo seeds / defaults rather
  than model truth — display-only now that the output cap is decoupled;
  catalog-driven windows are future work.

## Verdict

**SHIP** (pending the full gate + the installed-binary live probes,
which are the wave's acceptance gate).
