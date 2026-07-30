# W5e-1b — review of record #1 — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-e1b` @ `c6aa60d`.
Trigger: the OWNER's live v0.0.18 run, two screenshots. Neither bug was
visible to 1071 passing tests.

## Bug 1 — the TUI offered a method it never checked was served

Symptom: `invalid_argument — unknown session method` in the status bar; the
OAuth card stuck at "starting the loopback flow…".

Cause, in two parts:
- A `haiderd` running since **Jul 28** — five releases stale — correctly
  rejected `account.oauth_start`, a method that binary never had. Retired
  with SIGTERM (the store's persist-before-publish design makes that safe).
- The REAL defect: `Welcome` carries `features` and `daemon_version` and the
  TUI ignored both, so it rendered every affordance regardless of what the
  connected daemon could serve. This is the client half of report §4.1 —
  "clients hide/disable only the methods whose feature is absent" — which I
  raised as **P3-1 in my own W5c.2a review of record and did not implement**.
  It failed in the field exactly as that finding described.

Fix: `RpcClient` retains its negotiated `Welcome` (accessor `welcome()`);
`Link` captures features + version at handshake; `run_live` plants them on
the model; `AppModel::daemon_serves(feature)` gates, with
`stale_daemon_note()` naming the running version and the remedy. Demo mode
is always capable (it answers locally).

## Bug 2 — a non-durable request's failure had nowhere to land

With a CURRENT daemon the start was accepted, yet the card still hung.
`account.oauth_start` is deliberately non-durable, so its error reply carries
**no `command_id`** — the generic `LiveReply::Failed` path had nothing to
correlate and the card waited forever.

Fix: identity-tag from `CommandContext::oauth_attempt` into a new
`LiveReply::OAuthStartFailed`, and fail the card in place. This is precisely
the shape TUI6.4 solved for `vault.stage` (`StageFailed`); the same lesson
was re-learned because a NEW non-durable request was added without applying
it. Worth generalizing: **every non-durable request needs identity tagging
before it ships**, not after a field report.

## Reviewer self-audit (the finding I am least comfortable with)

My first pin for bug 2 called `AppModel::oauth_add_failed` directly — it
exercised the MODEL and left the LINK mapping unpinned. The mutation
(deleting the `oauth_attempt` branch) **SURVIVED**. That is exactly the
vacuity pattern I spent this session catching in codex's patches — a test
asserting a property against the wrong layer — appearing in my own work.
Rewritten to drive `map_response` directly; the mutation now kills. The
doc comment records the earlier failure so the next reader sees why the test
is shaped that way.

## Mutations (executed post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | Drop the `daemon_serves` guard from the OAuth add arm | KILLED |
| M2 | Delete the `oauth_attempt` branch from `map_response`'s Error arm | SURVIVED → real pin added → KILLED |

## Gate

clippy clean. Ledger 1069 → 1072. All 13 crates green (tui 491).

## Carried

`provider.models_refresh` and `provider.configure` affordances are not yet
feature-gated (no UI offers them yet). When W5e-3 adds the pickers, they gate
on `provider_models_v1` / `provider_configure_v1` — the same law, applied
before shipping rather than after.

## Verdict

**SHIP.**
