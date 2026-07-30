# W5e-2 — review of record #1 — SHIP

Reviewer: Fable 5. W5e-2a (`fdebb07`, discovery — built by Fable) +
W5e-2b (`1ca62dd`, persistence/RPC/registry — codex gpt-5.6 xhigh).
Authority: `docs/briefs/W5e-2-model-discovery.md`.

Owner requirement: model choice must come from the vendors' own sources,
"not hardcoded".

## Provenance (the point of the wave)

- **OpenAI**: `GET https://chatgpt.com/backend-api/codex/models` — the
  installed codex CLI's own catalog endpoint, confirmed 2026-07-30 against a
  live `~/.codex/models_cache.json` plus the binary's
  `codex-api/src/endpoint/models.rs` symbol. Parses their real payload:
  `display_name`, `description`, `default_reasoning_level`,
  `supported_reasoning_levels` (low→ultra), `visibility`, `priority`.
- **Anthropic**: `GET https://api.anthropic.com/v1/models` under the OAuth
  bearer + the beta headers W5b.2 already proves work for inference.
  NOT statically confirmable (the darwin `claude` binary is a packed 59 MB
  bundle with no readable strings and no on-disk model cache), so the
  fallback ladder is explicit: refusal → last-known cache → honest
  "unavailable". Verify the live status code against a real Claude Max token
  during W5e-3's picker work and record it.

## Verdict per criterion

1. **Never synthesizes — PASS.** `CatalogError::Unavailable` on refusal,
   malformed payload, empty list, oversized body, or redirect; the caller
   keeps its cache. A provider declaring no effort ladder yields an EMPTY
   ladder. Entries without an id are skipped, never guessed.
2. **SSRF discipline reused — PASS.** Discovery is credential-bearing to a
   fixed origin and takes the W5a path verbatim: `FixedOriginGuard`
   resolve-validate-pin, `.no_proxy()`, `redirect::Policy::none()`,
   `CONNECTION: close`, 1 MiB bounded streaming read.
3. **No lock across HTTP — PASS.** The actor resolves descriptor + cached
   ETag synchronously, then spawns an owned task for `broker.resolve` +
   discovery and returns the result via
   `AccountCommand::ProviderModelsRefreshCompleted`. The actor loop keeps
   servicing commands throughout. Independently mutation-verified below.
4. **Atomic cache + revision — PASS.**
   `put_provider_models_and_advance_management_revision` commits the row and
   the revision in ONE transaction (codex's own mutation #15 found the
   split-transaction partial-row bug and fixed it). 304 touches
   `fetched_at_ms` only, no revision bump.
5. **Registry serves discovered models — PASS; the W5c.2a P3 is CLOSED.**
   `available` now requires a non-empty DISCOVERED inventory, the default is
   filtered to discovered slugs, and `configure`'s validated-default checks
   against discovery rather than a literal list. A provider with no cache
   entry reports an empty inventory and unavailable — never a guess.
6. **Feature honesty — PASS.** `provider_models_v1` advertised only now that
   the method exists (the W5c.2a lesson applied without prompting).

## Audit integrity — mutations re-executed by the reviewer

codex reported 16 executed mutations. Three re-executed independently, chosen
for consequence; **all three KILLED at runtime**:

| # | Mutation | Result |
|---|---|---|
| M1 | Join the discovery task inside the actor arm (serialize HTTP into the loop) | KILLED — the concurrent `ResolveCredential` timed out |
| M2 | Touch the cache on `Unavailable` (re-upsert with a new timestamp) | KILLED — the exact cached-row equality caught the changed `fetched_at_ms` |
| M3 | Drop `&& !discovered_models.is_empty()` from the availability derivation | KILLED — `builtin_without_cached_models_is_unknown_not_available_with_guesses` |

M3 is the one that matters for the owner requirement: it proves a hardcoded
inventory cannot creep back in without a test noticing.

## Test-design note

No test asserts a real model slug as "the" list — fixtures use `frontier-a`
style names and assert SHAPE and PROVENANCE. A suite that hardcoded
`gpt-5.6` would reintroduce exactly what this wave removes; that rule is
recorded in the design brief as a standing constraint.

## Gate (reviewer-run, per-crate, clean tree)

clippy `--workspace --all-targets -D warnings` clean. Ledger 1052 → 1069.
All 13 crates green: provider 55 · daemon 166 · store 42 · rpc 50 · tui 486.
codex's 42 reported failures were all sandbox socket-bind denials.

## Verdict

**SHIP.** Discovery is genuinely the vendors' own data, never synthesized,
and the hardcoded-inventory P3 carried since W5c.2a is closed and pinned.
