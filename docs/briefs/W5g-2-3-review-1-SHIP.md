# W5g-2 + W5g-3 — review of record #1 — SHIP

Reviewer: Fable 5 (implementer too — UI work stays with Claude; the JWKS
pin is a review-P3 closure with a reviewer-authored oracle). Branch
`w5-g2`, reviewed at commit 41d2208 (frozen ref).

## W5g-2 — §5.3: the alias is a visible, editable field

- **API login card** grows an alias row: prefilled with the smallest free
  `«provider»-api[-N]` against the live snapshot (the slash command's
  token still prefills, case-folded — compat), tab moves the keystrokes
  between alias and masked key, input is grammar-filtered AT THE KEYBOARD
  (`[a-z0-9][a-z0-9._-]{0,63}`, uppercase folds), and a submit the daemon
  would bounce is refused in place — key preserved, offending field
  focused. The focus split is a SECRECY boundary: a key pasted while the
  alias is focused lands in the visible field's filter, never silently in
  a rendered row (pinned by test).
- **OAuth card**: a FAILED card now edits its alias in place and ⏎
  retries the whole flow under a fresh attempt — the §5.3 collision
  recovery. Digits are alias characters, so the `[1]`/`[2]` key map
  yields to typing there (⏎ retry · esc close); WaitingBrowser keeps its
  sim-parity keys. The card no longer needs a cancel-reopen loop to
  recover from an alias rejection.
- The alias-suffix scan is now ONE function (`smallest_free_alias`),
  shared by both cards.
- Sim-parity note: alias editing is an authorized W5 EXTENSION (the sim
  auto-generates aliases, report §5.3 names the extension explicitly).

## W5g-3 — the JWKS `.dns_resolver`-only removal now dies (W5b.2a P3)

`FixedOriginGuard::connection_resolution_count()` is a production surface
(the counter always existed at connection-resolve time; only its gate was
`cfg(test)`), the verifier stores its guard under `cfg(test)`, and the new
pin drives a REAL `verify()` against a TEST-NET-pinned resolver. The
verdict rides the resolution count, not the error code — a discovery made
executing it: this network's TLS-intercepting middlebox serves a real
JWKS for a TEST-NET address, so an error-code assertion would be
environment-dependent, while the count is zeroed by the mutation in every
environment.

## Mutations (reviewer-chosen, EXECUTED post-commit at 41d2208)

| # | Mutation | Result |
|---|---|---|
| M1 | delete ONLY `.dns_resolver(…)` (the P3's named residual) | KILLED by the new pin; the old private-DNS test stays green — exactly the blind spot the P3 described |
| M2 | prefill skips the taken-alias scan | KILLED (prefill test) |
| M3 | focus routing removed (all chars → key) | KILLED twice (tab-routing + submit tests) |
| M4 | submit sends `alias: None` | KILLED (submit test) |
| M5 | grammar gate on ⏎ removed | KILLED (refusal test) |
| M6 | FAILED-card ⏎ cancels (old key map) | KILLED (retry test) |

## Gate

Workspace clippy `-D warnings` clean; haider-tui full suite green; full
per-crate gate `gate16.out`; ledger 1124 → 1131.

## Honest residuals (non-blocking)

- The `provider.configure` card (custom provider create/edit form) is the
  remaining §5 UI item — deferred to its own patch; the RPC and daemon
  sides shipped in W5c.2b.
- The login card's Failed-with-empty-key row still doubles as the error
  row (pre-existing layout); the alias row is unaffected.

## Verdict

**SHIP** (merge to main, ships as v0.0.25).
