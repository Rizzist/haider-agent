# W5g-8 — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w5-g8`, reviewed at 509c098 (frozen ref).
Implementer: codex lane (gpt-5.6 xhigh) per the brief; the uncertainty
split below is the reviewer's, forced by the host gate.

## The live facts that shaped the design

Probed against `auth.openai.com/oauth/token` with the real grant: our
exact refresh shape WORKS (200; scope superset passes; extra fields
tolerated) and the response ROTATES the refresh token. The imported
token is therefore SHARED STATE with the codex CLI: whichever client
refreshes first invalidates the other's copy — that is precisely how
the owner's account died (codex refreshed; our copy went
`invalid_grant`; the old resolve tombstoned the mark forever).

## The three laws

1. **Source-first.** An expired imported credential re-reads its source
   file through the REAL import handler (actor-owned, internal frame
   sink, fresh receipt ids — single-writer law intact) and commits the
   fresh grant. We never race the external CLI for the rotating token.
2. **Refresh fallback** only for expiry WITNESSED in this process
   (bundle aged out / provider 401); the rotation persists.
3. **The mark is uncertainty.** A snapshot-EXPIRED mark may record a
   forced shutdown mid-exchange; replaying the rotating token under it
   risks reuse-detection revoking the WHOLE grant family (including the
   external CLI's successor). Under the mark: source may heal, refresh
   never runs, and the terminal state is the NAMED remedy —
   `credential expired — re-run \`haider import codex\` or sign in
   again`.

## What the host gate caught (the review working)

The lane's first draft refreshed under the mark and broke
`forced_shutdown_never_retries_an_uncertain_refresh_on_successor` — the
pre-existing safety law the sandbox could not run (socket-bound). The
uncertainty split reconciles healing with that law; the lane's four new
pins were re-anchored (natural expiry keeps the fallback; the marked
case pins ZERO endpoint calls).

## Mutations (reviewer-chosen, EXECUTED post-commit at 509c098)

| # | Mutation | Result |
|---|---|---|
| M1 | tombstone restored (mark fails resolve immediately) | KILLED (taxonomy pin — healing never ran) |
| M2 | uncertainty split removed (refresh under the mark) | KILLED (forced-shutdown successor law) |
| M3 | heal answers NotImported unconditionally (source never read) | KILLED twice (concurrent single-flight pin + taxonomy pin) |

## Live acceptance (2/2)

Doctored expired import (`HAIDER_CODEX_AUTH_PATH` copy with a past-exp
JWT) → source file freshened AFTER the stale grant stored → a typed
turn silently self-heals and STREAMS — no manual re-import, no token
rotation consumed (the codex CLI stays healthy; the probe restores the
real import afterwards).

## Gate

Workspace clippy `-D warnings` clean; full daemon lib suite 151/151 on
the host (socket laws included); full per-crate gate `gate23.out`;
ledger 1155 → 1159.

## Honest residuals (non-blocking)

- Claude-code imports share the machinery; the anthropic token endpoint
  path is covered by fakes but still not live-verified (needs the
  owner's Claude Max login).
- A marked account with a still-valid bundle heals via source or
  refuses; serving the still-valid access token under a mark was
  considered and deferred (the mark means something failed).

## Verdict

**SHIP** (merge to main, ships as v0.0.30).
