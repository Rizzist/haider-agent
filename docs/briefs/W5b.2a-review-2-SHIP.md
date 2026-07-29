# W5b.2a review — round 2 (JWKS origin closure) — SHIP

- Frozen SHA: `c96c34e` (branch `w5-b2`). Method: dual review — gpt-5.6 (ordering subagent) + Fable (code read + independent socket gate + mutation probing).
- Gate (Fable env): daemon lib 106/0 (3× stable, no abort), both JWKS pins pass, all crates green per-crate; clippy -D warnings + fmt clean; baseline 999. (`cargo test -p haider-daemon` all-targets SIGABRTs on the loaded box — environmental resource abort, does not reproduce per-binary.)

## Verdict: SHIP

- **JWKS origin fully closed.** `OpenAiIdentityVerifier::verify` builds a `FixedOriginGuard` for `auth.openai.com:443`, attaches it as the reqwest `dns_resolver`, `.no_proxy()`, and `validate_endpoint`s (resolve+validate+pin, private/link-local/metadata/IPv4-mapped/RFC6598 blocked) BEFORE any TCP connect. Independent ordering review agrees: no connect-before-block gap; no skippable validation; one-time resolution established before the client can connect. Identity forgery via DNS/hosts rebinding is closed.
- **P2 bounded read correct.** Chunked JWKS with no Content-Length stops at the first over-limit chunk without consuming the remainder; reject before buffering past `TOKEN_RESPONSE_LIMIT`.
- **No token-path regression.** `bounded_response` reuse for the (public) JWKS body left the token exclusive-ownership/zeroize invariant unchanged; the `token_response_source_chunks…` source pin correctly updated 2→3 CONNECTION:close (exchange, refresh, JWKS).
- No new secret exposure; exposing `FixedOriginGuard` from haider-provider widened no surface; prior W5b/W5b.2 invariants intact.

## Non-blocking follow-up (P3 — test strength, code is safe)

- **[P3]** `crates/haider-daemon/src/oauth_tests.rs:512` — `openai_jwks_private_dns_answer_is_rejected_before_key_use` catches FULL guard removal but not deleting ONLY `.dns_resolver(...)`: with the static stub, preflight private-IP rejection stays green while a public-first/private-second DNS rebind would become reachable at connection time. The CURRENT code is safe (both the preflight block and the pinned resolver are present). Follow-up: strengthen the pin to also kill a `.dns_resolver(...)`-only removal — e.g. a stub answering public-then-private and asserting the pinned (preflight) address is used with no second resolution (the guard already exposes `connection_resolution_count()` under cfg(test); the verifier would need to surface its guard to the pin). Non-blocking for v0.0.15; tracked for the next OAuth-touching lane.

## Next
Merge W5b.2 (branch w5-b2) → main. Then W5c (account/provider management actor + receipts + resolver broker) → W5d + W4a3 batched Claude/Fable TUI.
