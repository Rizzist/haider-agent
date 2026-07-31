# W8a — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w8-perms`, reviewed at 15c30b4 (frozen ref).
Implementer: codex lane (gpt-5.6 xhigh) per
docs/briefs/W8a-permissions-core-brief.md, mapped by
docs/research/w8-permissions-research.md.

## What shipped

The W4 `EffectBroker` remains the ONE approval authority; W8a
consolidated around it. Canonical daemon-owned tool registry (frozen
`ToolManifest`) sources both the advertised provider set and a read-only
inventory snapshot — advertised == dispatchable is a pinned law.
Providers now see `process_exec` (never `exec`); the dispatcher still
routes legacy `exec` for recovered history. Permission menus park in
`RunState::PermissionRequired`; recovery dual-reads historical
`InputRequired + MenuKind::Permission` checkpoints (a canonical
`PermissionRequired` for a non-permission menu is refused — corruption,
not tolerance). New receipt-backed `shell.exec` RPC: a synthetic run
owns the session (idle-only, typed Busy otherwise), the broker journals
`PreAuthorized(UserTyped)` → Dispatched → Outcome, emits
`CommandExecution` + byte deltas, creates no UserMessage and zero
provider requests; `!cd` is a typed rejection this slice. Durable
permission-grant inventory projection for W8b's /tools. Ledger
1212 → 1226.

## Mutations (reviewer-chosen, EXECUTED post-commit at 15c30b4)

| # | Mutation | Result |
|---|---|---|
| M1 | legacy `exec` dispatcher routing dropped | first run SURVIVED a bad name filter (reviewer artifact — the pin is `canonical_inventory_equals_advertised_dispatchable_set`); isolated, re-run: KILLED |
| M2 | advertised name diverges from dispatchable (`process_exec2`) | KILLED (full daemon suite) |
| M3 | dual-read dropped (old InputRequired checkpoint refused) | KILLED |
| M4 | user-typed provenance journals plain `Allow` | KILLED in BOTH layers (tools suite + daemon shell suite) |
| M5 | changed body under a reused command id accepted (digest check dropped) | KILLED (store layer) |
| M6 | idle-only busy gate dropped | KILLED (store layer) |

## Honest residuals (non-blocking)

- UDS integration tests compile but cannot bind sockets in the codex
  sandbox — host gate is the authority (standing).
- The `!` user-facing parser, /tools screen, PermissionRequired badge
  polish, and model-`process_exec` output visibility are W8b (TUI lane).
- Containment is authorization, not sandbox (research risk 2) — UI copy
  in W8b must say "workspace cwd + bounded supervised process".

## Gate

gate35: full per-crate gate GREEN (fail=0) — daemon 206, daemond 88, store 48, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (merges with W8b as v0.0.36).
