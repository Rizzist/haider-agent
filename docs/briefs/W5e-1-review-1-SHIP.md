# W5e-1 — review of record #1 — SHIP

Implementer AND reviewer: Fable 5 (UI never goes to codex). Branch `w5-e1`.
Owner asks (2026-07-30, screenshot): move the `+` buttons to the bottom,
hover-highlight them, and make OAuth actually work in the real TUI.

## The three asks

1. **Bottom-anchored add row — DONE.** The empty-state exposed the flaw:
   "one global add row AFTER all groups" is sim-correct, but with ZERO
   groups that lands at the top under the empty-state line. The add row +
   hints are now fixed bottom chrome (`footer_lines` drawn at
   `area.bottom − height`), the list keeps the top and truncates behind the
   footer rather than colliding. Pinned by
   `add_row_is_bottom_anchored_even_when_empty`.
2. **Hover — DONE.** Account rows take the hover band on mouse-over (was
   keyboard-cursor only) and each add BUTTON takes hover ink on its own
   column rect (per-button hit rects, not one row-wide rect).
3. **OAuth actually works — DONE (live).** The daemon engine has been live
   since W5b.2; the TUI stub was the missing half. Now:
   `account.oauth_start` → the runtime opens the browser at the
   daemon-supplied URL → `account.oauth_status` polls on the driver's own
   deadline clock (1.5 s cadence via `next_deadline`, the same bounded-wakeup
   discipline the login card needed) → `Ready` mints a durable
   `account.add` (outbox; the reference is excluded from the semantic digest
   so a retry replays the committed descriptor) → the card closes on the
   descriptor and rows REFRESH FROM THE DAEMON (no local insert).

## Laws carried

- **Attempt gating** (the login-card law): every card reply is matched on the
  client-side attempt; a retired card's ghosts — phase, failure, even a late
  completion — touch nothing. Pinned by `cancel_closes_and_late_replies_are_ghosts`.
- **No fabricated URL**: the authorize URL only ever comes from the daemon's
  sanctioned registration; `AppRequest::OpenUrl` is a runtime effect (like
  CopySelection), never a wire command.
- **Alias derivation (§5.3)**: provider name + smallest free numeric suffix
  against current rows; the daemon re-checks uniqueness at commit.
- **Demo parity**: `[1]` simulates the authorize exactly like the sim's
  confirmAuth — row lands selected under the sim provider name — so both
  modes drive the same reducer seams.

## Mutation check (executed post-commit)

Skip the taken-alias scan (always the bare provider name) →
`oauth_alias_derives_the_smallest_free_suffix` FAILED at runtime. KILLED.

## Gate

clippy `-D warnings` clean. Ledger 1047 → 1052. All 13 crates green
(tui 486).

## Not yet proven

The live end-to-end authorize (real browser, real ChatGPT consent, real
`account.add`) is NOT covered by a test — it cannot be. The install probe
covers the demo card; the live path gets an owner-driven probe on the
v0.0.17 install. Stated plainly rather than implied by a green suite.

## Verdict

**SHIP.**
