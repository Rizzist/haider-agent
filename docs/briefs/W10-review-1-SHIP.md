# W10 — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w10-mgmt`. W10a implementer: codex lane
(52cefc5); W10b implementer: Fable (168a4c8, UI lane).

## What shipped

- **provider.remove** (W10a): receipt-backed, revision-fenced custom
  removal. Builtins refuse typed ("release-owned"); providers with
  referencing credential aliases refuse NAMING the aliases (no cascade —
  removal never destroys credentials); the removal receipt beats restart
  resurrection (mirroring account.remove's reconciliation);
  `provider_remove_v1` advertised. Ledger → 1299.
- **Account remove UI** (W10b): `x` arms an inline confirm on /accounts
  (Enter confirms → durable revision-fenced `account.remove`; esc
  disarms), the committed reply prunes the row, follows the daemon's
  replacement-active choice, and refreshes both snapshots; demo removes
  locally with zero live requests.
- **Provider remove UI**: same arm/confirm on /providers behind
  `provider_remove_v1`; the daemon's typed refusals (builtin, blocking
  aliases, revision conflict) surface verbatim via the correlated
  Failed path — the client never pre-judges (no provenance marker
  exists on the wire; the daemon is the authority).
- **Edit card**: `e` opens the custom card prefilled from the summary
  with identity locked (name/origin display-only, focus pinned to the
  model line) riding provider.configure's exact-match update semantics.
- **HuggingFace card**: the stale "lands with W5e" stub (W5e shipped
  months of releases ago) is retired — `h` (and the accounts add-kind)
  opens the custom card preset to `https://router.huggingface.co/v1`,
  openai-compatible, smallest-free `huggingface` alias; the normal
  login flow adds the token. Ledger → 1304.

## Mutations (EXECUTED post-commit)

W10a (at 52cefc5):
| # | Mutation | Result |
|---|---|---|
| P1 | builtin guard dropped | KILLED |
| P2 | blocking-accounts guard dropped | KILLED |
| P3 | durable JSON save dropped | SURVIVED single-layer — ISOLATED: with BOTH the save AND the receipt reconciliation disabled, the resurrection law FAILS; the receipt layer alone carries the law (true defense-in-depth, accepted with isolation evidence per doctrine) |

W10b (at 168a4c8):
| # | Mutation | Result |
|---|---|---|
| B1 | accounts `x` arm dropped | KILLED (2 tests — live + demo) |
| B2 | provider Failed correlation dropped | KILLED (refusal-surface law) |
| B3 | edit identity lock dropped | KILLED |

## Gate

gate39: full per-crate gate GREEN (fail=0) — daemon 210, tui 552, rpc 57, store 50, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (v0.0.38). · ledger 1304.
