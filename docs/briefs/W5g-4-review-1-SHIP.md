# W5g-4 — review of record #1 — SHIP

Reviewer: Fable 5 (implementer too — UI stays with Claude). Branch
`w5-g4`, reviewed at commit 2d96ac2 (frozen ref).

## What the patch does

`+ Custom (OpenAI-compatible)` was a dead button; the daemon's
`provider.configure` has been live since W5c.2b with no front door. Now:

- **Demo** renders the sim's MenuBox verbatim (info lines, `[1] add
  http://127.0.0.1:8000/v1 (demo)`, `[2] cancel`) and `[1]` executes
  confirmAuth's exact custom arm — `custom-N` / `local-N` on the fixed
  URL, hex-suffixed identity, selected, the sim's ✓ message.
- **Live** is the authorized §4.4 extension: editable name (alias
  grammar, prefilled smallest-free `custom[-N]` against the registry) and
  origin fields, tab cycles, ⏎ submits `provider.configure` as a CREATE
  (chat-completions family, api-key auth, enabled, models discover) under
  the snapshot-revision CAS. A commit chains straight into the masked key
  card — a provider without a credential is a dead end, so the flow
  refuses to leave one. A failure reopens the fields with the public
  reason; a `revision_conflict` also refreshes the provider snapshot so
  the retry submits under fresh truth.
- Feature-gated on `provider_configure_v1` (§4.1: never offer what the
  daemon cannot serve); the two accounts-screen cards are mutually
  exclusive; the origin string rides the wire as data only — never a
  shell/browser interpolation (§4.4).

## Mutations (reviewer-chosen, EXECUTED post-commit at 2d96ac2)

| # | Mutation | Result |
|---|---|---|
| M1 | demo `[1]` lands bare `custom` (sim recipe broken) | KILLED |
| M2 | submit sends `expected_revision: 0` | KILLED |
| M3 | commit closes the card without the key-card chain | KILLED |
| M4 | error correlation dropped (card stuck in Submitting) | KILLED |
| M5 | feature gate dropped (offered to a stale daemon) | KILLED |

## Gate

Workspace clippy `-D warnings` clean; haider-tui suite green; full
per-crate gate `gate17.out`; ledger 1131 → 1137.

## Honest residuals (non-blocking)

- EDITING an existing custom provider (safe-update path of
  `provider.configure`) has no UI yet — the card only creates. The RPC
  supports it; a later patch can open the card prefilled from a
  `/providers` row.
- Model discovery for the new provider rides the manual `/providers`
  refresh; the automatic refresh trigger is still OAuth-scoped
  (W5f-2d) — extending it to api-key accounts is a one-line candidate for
  the next lane, kept out of this patch's blast radius.
- HuggingFace's add button still flashes its placeholder (it needs a
  token + endpoint pair — a different card).
- No live daemon round-trip probe for configure yet (needs a running
  OpenAI-compatible server to be meaningful); the wire shapes are pinned
  by fixtures and the driver by tests.

## Verdict

**SHIP** (merge to main, ships as v0.0.26).
