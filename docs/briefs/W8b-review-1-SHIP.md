# W8b — review of record #1 — SHIP

Reviewer + implementer: Fable 5 (UI lane never goes to codex). Branch
`w8-perms`, reviewed at f1ee995.

## What shipped

- **Literal `!` escape** (new vocabulary — research proved the sim never
  had one): session-screen composer strips EXACTLY one leading `!` and
  routes the literal rest to the receipt-backed `shell.exec`
  (LiveCommand::ShellExec → RequestBody::ShellExec, cwd None). `!` alone
  is a validation flash; `!!x` sends the literal `!x`; a `!` line is
  NEVER a provider turn (no Submit, no UserMessage). Demo mode flashes
  honestly — the six bare VFS commands stay the demo's only shell. The
  committed `CommandExecution` events render the row (no optimistic
  shell row).
- **Live `/tools`**: the D1-2 law evolved, not retired — still no
  locally minted card; live `/tools` now opens the read-only
  Screen::Tools and issues the `tools.inventory` READ. Rows render the
  committed snapshot (name · effects · default) + remembered session
  grants; in-flight reads say "fetching" — nothing fabricated; a stale
  reply for another session is dropped. Containment copy is honest:
  "workspace cwd + bounded supervised process, not a sandbox" (research
  risk 2).
- **Process output visibility** (research risk 10): a `ToolCall`'s
  durably retained `CommandOutput` tail now renders under the tool row
  with the same truncation/decode honesty markers as direct command
  rows.
- `? PERMISSION_REQUIRED` badge: already live from the sim port —
  W8a's state migration lights it up; no TUI change needed.

## Fixture evolutions

`live_mode_opens_no_local_card_it_has_no_way_to_close` — `/tools` left
the refusal loop (it has a real read now); `/voice` + `/say` keep the
law; a dedicated W8b test pins the /tools read + no-card halves.

## Mutations (EXECUTED post-commit at f1ee995) — all KILLED

| # | Mutation | Killed by |
|---|---|---|
| B1 | `!` arm dropped from composer submit | live routing law AND demo flash law (both branches share the seam) |
| B2 | live /tools back to refuse_demo_only | the inventory-read law |
| B3 | ToolCall output block dropped | the retained-tail law |

## Gate

gate36: full per-crate gate GREEN (fail=0) — tui 547, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (v0.0.36). · ledger 1226 → 1231.
