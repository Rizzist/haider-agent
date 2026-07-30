# W5f-1 — review of record #1 — SHIP

Implementer: codex (gpt-5.6 xhigh, ~808k tokens). Reviewer: Fable 5.
Branch `w5-f1` @ `a16fb28`. Authority: docs/briefs/W5f-1-import-brief.md.

## What landed

`account.oauth_import` — the daemon consumes stored OAuth from installed
CLIs (codex `~/.codex/auth.json`, Claude Code `~/.claude/.credentials.json`,
env-overridable) into first-class `openai-oauth` / `anthropic-oauth`
accounts. `haider import [codex|claude-code]` drives it headlessly;
`account_oauth_import_v1` rides the welcome features.

## What the review verified (not just read)

- **Seam reuse is real.** The import routes through the SAME
  `persist_oauth_bundle` + `finalize_oauth_commit` seam as loopback PKCE,
  with receipt preflight/claim (idempotent replay), reserved-alias +
  pending-login fences, replace-vs-add guarded by provider/auth-method,
  generation increment from the prior bundle, and refresh-fence
  invalidation on replace. No parallel commit path exists.
- **Refresh compatibility by construction**: issuer/audience/scopes come
  from the SANCTIONED provider registration, never from the file.
- **Secret hygiene**: file bytes and tokens ride `Zeroizing` end to end
  (`SecretJson` visitor); receipts carry only `{source, alias, provider}`;
  malformed-JSON errors name path + line/column, never content; no new
  `Debug` derives near token-bearing structs.
- **Bundle format change is backward-compatible**: the
  refresh-on-first-use marker is a trailing tagged suffix — old vault
  bundles decode as unmarked; a refreshed bundle clears it durably.
- **Alias incarnation care** I did not ask for: a historical import
  receipt is not trusted to prove source ownership — the latest committed
  revision per alias is the authority, so an alias removed and reused by a
  loopback add is NOT silently replaced (pinned by its own test).
- **The fence test drives the REAL `start_account_actor`** (the W5c.2b
  lesson checked explicitly — no fence-reimplementing double).

## Mutations (chosen and executed independently by the reviewer)

| # | Mutation | Result |
|---|---|---|
| M1 | `alias_has_pending_login_reservation` always answers false | KILLED |
| M2 | codex bundle stamped with a non-sanctioned issuer | KILLED |
| M3 | re-import forgets `refresh_fences.invalidate` | KILLED |
| M4 | broker ignores the imported fallback marker | KILLED |

## Gates (re-run independently)

haider-rpc / haider-accounts / haider-cli green inline; haider-daemon
green detached (EXIT=0); workspace clippy `-D warnings` clean. Ledger
1084 → 1097.

## Notes

- The CLI's `import_command_id` is fresh per invocation (pid+nanos) — an
  interactive one-shot; a network-lost response is re-runnable and the
  receipt path replays committed work. Acceptable.
- Claude import `subject_hash` derives from the access token (rotates per
  import) — display-only today; noted, not blocking.
- macOS Keychain deliberately NOT read (daemon GUI prompt trap) — Claude
  import needs the credentials FILE or the (now working) browser flow.

## Verdict

**SHIP.**
