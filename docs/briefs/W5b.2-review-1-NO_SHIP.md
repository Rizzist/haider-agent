# W5b.2 review — round 1 — NO_SHIP

- Frozen SHA: `6deeafa` (branch `w5-b2`), diff `01e01f7..6deeafa`.
- Method: dual review of record — Fable (code review + independent per-crate gate + 2 load-bearing mutation kills: x-api-key-absence, SSRF fixed-origin private-IP) + gpt-5.6 correctness lens (2 parallel subagents: leak-path + identity).
- Gate (Fable env): per-crate all green (provider 47, daemon 139, daemond 86); clippy -D warnings + fmt clean; baseline 991→997.
- Agreement: the subscription INFERENCE adapters are clean — no wrong-origin bearer path via redirect, proxy, DNS rebinding, IPv4-mapped IPv6, or endpoint override. Constants, encodings (OpenAI form auth-code / JSON refresh), dispatch, and prior W5b invariants intact.

## Blocking findings (VERDICT: NO_SHIP)

- **[P1] OpenAI JWKS fetch bypasses the fixed-origin guard** — `crates/haider-daemon/src/oauth.rs:361`. `OpenAiIdentityVerifier::verify` builds a reqwest client with `.no_proxy()` + redirect-none + timeouts but NO `FixedOriginGuard` (no `dns_resolver` pin, no private-IP block) before `GET https://auth.openai.com/.well-known/jwks.json`. DNS/hosts rebinding of the JWKS host to a private HTTPS endpoint presenting a locally-trusted `auth.openai.com` cert substitutes attacker signing keys → a forged id_token validates → identity forgery on the `VerifiedIdToken` trust anchor. Fable-confirmed at oauth.rs:361-372. Fix: route the JWKS fetch through the same resolve-validate-pin fixed-origin guard used by the inference adapters (host `auth.openai.com`, HTTPS/443, block private/link-local/metadata/IPv4-mapped IPs, pinned addresses through connection). Add a test seam (injectable resolver + endpoint) — the JWKS path is currently untested.

- **[P2] JWKS size limit applied after full buffering** — `crates/haider-daemon/src/oauth.rs:382`. The `content_length()` pre-check (oauth.rs:376) is skipped for a chunked response (`None` → false), so `response.bytes()` buffers the entire body before the post-check at :386 → memory-exhaustion DoS via a large chunked JWKS. Fix: read the body with the existing streaming `bounded_response` helper (oauth.rs:1839) which caps at `TOKEN_RESPONSE_LIMIT` chunk-by-chunk.

## Required for W5b.2a SHIP

Fix both. Add pins: (a) a JWKS host resolving to a private/loopback/metadata IP is rejected before any key is used (revert of the guard fails it); (b) an oversized chunked JWKS is rejected without full buffering (revert to `.bytes()` fails it). Re-run ONLY these new JWKS mutations + the existing openai-oauth identity pin — do NOT re-run the full W5b/W5b.2 audit (already verified; a full re-run thrashes disk). Per-crate socket gate. Re-review r2 focuses the JWKS origin closure.
