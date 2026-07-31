# W5g-7 — review of record #1 — SHIP

Reviewer: Fable 5. Committed directly on main at 2b2c3e6 (frozen ref; the
per-patch branch ritual was skipped this once — journaled).
Research: gpt-5.6 xhigh read-only deep lane (owner-directed), findings
independently verified by executed mutations and a live click-truth
probe at the owner's exact 80×24 geometry.

## Hover — the compact-banner shift (root cause, CONFIRMED)

My static alignment dump showed hit rects perfect at 118×40 — the
research found why the owner still saw the offset: on short terminals
(the owner's is 80×24) the 4-row launcher banner COMPACTS to one line,
and the recorded hit rows never learned — every launcher hit rect sat
exactly 3 rows below its painted row. The compaction now reports its
scalar shift (exact by construction: only the head block is ever
replaced, and no hit row lives inside it) and the visible-row conversion
applies it. Alignment pinned on BOTH paths; the mutation prints the
literal 3-row delta.

Two confirmed secondary defects fixed with it:
- **Stationary-pointer drift**: a redraw that moves targets under a
  parked pointer kept painting the OLD target's highlight at its new
  row (identity-only cleanup). The post-draw settle clears a highlight
  the pointer no longer resolves to — and deliberately NEVER adopts the
  new target, or every keyboard-driven redraw would let a parked pointer
  steal palette/menu navigation.
- **Resize race**: a queued Moved could re-arm hover from the dead
  pre-resize map; the map is invalidated the moment the resize event is
  seen.

Journaled, not fixed (lowest-ranked finding): launcher hit rects span
the full content width rather than the centered column — cosmetic
over-generosity, never a row offset.

## Anthropic OAuth consent parity (research Q2, CONFIRMED)

Same public client id all along; the consent delta was purely SCOPES.
haider asked for `user:inference` alone (2 consent items); Claude Code
2.1.220 asks for six, and the registration now carries that exact set in
its exact order. `user:inference` remains the only scope the turn path
consumes; the token guard's all-scopes law means pre-parity grants need
one re-login (the Anthropic account was never live-verified anyway).
Endpoint/redirect-shape differences vs Claude Code's flow were reported
and deliberately NOT adopted — our loopback PKCE works end to end and
the owner's ask was consent parity.

## Mutations (reviewer-chosen, EXECUTED post-commit at 2b2c3e6)

| # | Mutation | Result |
|---|---|---|
| M1 | compact shift dropped from the visible conversion | KILLED — the pin prints rect 14 vs painted 11, the exact +3 |
| M2 | settle reverts to identity-vanish cleanup | KILLED (moved-target pin) |
| M3 | settle ADOPTS the resolved target | KILLED (keyboard-theft pin) |
| M4 | scopes back to `user:inference` alone | KILLED (registration pin + factory fixture guard) |

Live acceptance: at 80×24 (compaction active) clicking the PAINTED
`⚿ Accounts` row opens `/accounts` on the installed build.

## Gate

Workspace clippy `-D warnings` clean; full per-crate gate `gate21.out`;
ledger 1150 → 1154.

## Verdict

**SHIP** (merge to main, ships as v0.0.29).
