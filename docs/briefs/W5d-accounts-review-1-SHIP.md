# W5d `/accounts` — review of record #1 — SHIP

Implementer AND reviewer: Fable 5 (UI never goes to codex; the owner's
review rule makes every review pass Fable regardless). Branch `w5-d` @
`435d471`. Sim law: `next-diffforge/src/pages/tui.js:3588-3688` (screen),
`2160-2168` (useAccount), `2516-2519` (Esc), `146-154` (seed). Report §5.1.

Because implementer and reviewer coincide here, the discipline is carried by
the executable parts: sim-line-verified parity claims, the mutation checks
(executed, runtime kills), and the honest divergence list below.

## What is 1:1 sim parity (verified against the sim source, not memory)

- Hierarchy: head line → optional action message → provider groups in
  first-seen order → rows (`●/○ alias [AUTH_LABEL] · identity · status
  [· in use]`) → ONE global add row after ALL groups → hints.
- Group header carries the FIRST base URL any of its accounts has
  (tui.js:3596-3599).
- `AUTH_LABEL`: oauth → `oauth`, api → `api key` (tui.js:145).
- Re-clicking the ACTIVE row re-emits `✓ provider → alias · label · active`
  with no daemon round-trip — the sim's `useAccount` has no early return.
- Entering the screen clears the stale action message (sim `startLogin` +
  screen-entry behavior).
- Esc: card open → card cancels (total modality); else session if attached,
  else launcher (tui.js:2516-2519).
- Demo seed: the sim's seven accounts VERBATIM, including the two base-URL
  carriers; `SEED_ACCOUNTS`/`SEED_ACCOUNT_PROVIDERS` pinned equal to the
  seed list.

## Deliberate divergences (all report-mandated or additive)

1. **Optimistic selection is forbidden** (report §5.1) — the sim flips
   `selected` locally because it has no daemon. Ours sets `pending_select`,
   requests `account.set_active`, and moves the dot ONLY on the correlated
   reply or a newer snapshot; both paths revision-gated. The pending row
   shows a pulsing `…` so the click still has visible feedback.
2. **Additive status vocabulary**: `Limited`→`rate-limited`, `Expired`,
   `Revoked` render honestly; unusable rows refuse selection locally with an
   actionable message (no doomed RPC). Sim has only `active`.
3. **Keyboard cursor** (↑/↓/Enter) — W5 accessibility extension, hover-band
   highlight, same code path as click.
4. **Add-row buttons**: API-key buttons open the existing masked `LoginCard`
   (TUI6 total modality preserved); OAuth/HF/custom are honest stubs naming
   W5e — the sim's button ROW is ported, unbuilt machinery is not faked.

## Wire

`LiveCommand::AccountList` (read, never outboxed) and
`LiveCommand::AccountSetActive` (durable, outbox + replay). The driver
correlates failures to the exact pending row via `pending_account_select`
and releases the gate with the public reason; retired/duplicate replies
retire through the same outbox discipline as every durable command. Demo
mode answers through the SAME reducer seams (`apply_snapshot` /
`apply_account_selected`), so the gates run in both modes.

## Mutation checks (executed, runtime kills)

| # | Mutation | Result |
|---|---|---|
| M-UI-1 | `select_account` flips `selected` locally before requesting (the exact naive-port bug) | KILLED |
| M-UI-2 | `apply_account_selected` applies unconditionally (revision gate dropped) | KILLED |

First execution attempt used sloppy perl anchors and — worse — `git
checkout` reverts against an UNCOMMITTED tree, which destroyed the patch's
app.rs work and forced a full reconstruction. The commit-before-mutation
rule from the W5c.2a incident now has two data points; it is no longer
advisory. Both mutations were re-executed properly against the commit:
clean single-anchor edits, runtime failures observed, tree restored to HEAD.

## Gate

clippy `--workspace --all-targets -D warnings` clean. Ledger 1031 → 1038
(7 new tests). Full per-crate gate green (tui 472).

## Not in this cut (tracked, not forgotten)

- `/providers` — owner design gate (§5.2); layout proposed to the owner,
  built next with the golden marked provisional until the install-probe
  sign-off.
- Dynamic arg slots (§5.3) + the editable alias field — after `/providers`.
- Launcher Accounts blurb still quotes seed counts in live mode; becomes a
  live projection when the launcher fetches the snapshot (W5d follow-up).

## Verdict

**SHIP.** Sim hierarchy verified line-by-line, the two laws that make this
screen daemon-truthful are pinned and mutation-killed, and every divergence
from the sim is either report-mandated or honestly additive.
