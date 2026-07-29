# W5b OAuth security review — NO_SHIP (P0 none; 4 P1 in refresh/lifecycle; sweep + mutation-audit gaps)

Reviewer: gpt-5.6 (codex), frozen 96f0c00. The engine CORE is sound: P0 none — constant-time state (mutation-killed), exact 127.0.0.1:0 numeric bind, random path/state/verifier/nonce, GET/Host/path validation, no redirects, 8 KiB/256 KiB bounds, PKCE S256, same-UID+Control gate layered before framing (connection.rs:875), ready-refs connection/instance/attempt-bound + zeroized on drop, R7 no-await holds, INV-1/2/R9/R12/R13/R14 no regression. But the refresh path + task lifecycle have real blockers.

REQUIRED FIXES (W5b.1 — security hardening, BEFORE W5b.2 adds real tokens):
- **P1-1 barrier ownership** (oauth.rs:456/887/1630/1732, runtime.rs:521): the coordinator start/callback + refresh-leader tokio::spawn handles are DISCARDED; shutdown cancels flows but joins none, and the broker has no shutdown/join. A blocked old refresh overlapping a restarted daemon → token-family revocation; callback tasks retain state/verifier/nonce/token past Stopped. Fix: own + join these tasks under the barrier (the W3b1 writer-join discipline); a coordinator/broker shutdown that cancels AND joins, bounded by the barrier deadline.
- **P1-2 late-failure fence bypass** (oauth.rs:1824/1831/1846/1968, accounts.rs:627): invalid_grant/permanent/validation failures call ALIAS-ONLY mark_expired before comparing the captured fence — a delayed failure after remove/re-add or a newer same-alias generation marks the REPLACEMENT descriptor (even another provider/method) Expired. Fix: fence the FAILURE path like the success path (compare captured generation; never expire a replacement).
- **P1-3 durable rotation-failure expiration** (accounts.rs:692/699, oauth.rs:1884): after server rotation + failed vault persist, mark_refresh_expired ignores descriptor-status persistence failure → the old invalidated refresh token is retryable on restart (token-resurrection, violates §3.5). Fix: durably persist the expired status; a persistence failure fails CLOSED (tombstone; the credential must not resolve to the dead token). Test with INJECTED production descriptor-persistence failure (not the in-memory status actor).
- **P1-4 refresh issuer/audience/resource binding** (oauth.rs:1995/2007/2044): refresh sends no audience/resource, ignores them if returned, validates only bearer/expiry/scopes then copies the prior issuer/identity — a refresh response with an unexpected issuer/resource is ACCEPTED. Fix: refresh validates issuer/audience/resource match the original bundle; a terminal-mismatch is rejected. THIS BLOCKS W5b.2 (real subscription tokens).

P2 (fold in): P2-1 scrub the token-response source Bytes (not just the Zeroizing aggregate — every allocation with token/error bytes); P2-2 complete the secret sweep (sweep BEFORE WAL cleanup, capture tracing + TUI output, trigger the RAW_ERROR sentinel, check state/verifier vs the success HTML); P2-3 add the missing §7.2 security tests (independent state/nonce/path, ACTUAL different-UID UDS peer, initial-token-before-vault on account-add, late-failure-after-replacement, shutdown/restart refresh overlap, production receipt/vault crash boundaries — these tests are what would have caught the P1s); P2-4 refresh-token omission (missing token / missing refresh_expires_in handled without erasing refreshability or a known expiry); P2-5 refresh CAS fences on IDENTITY/generation, not benign status/selection fields (a concurrent status change must not strand a rotated credential).
P3: require a nonempty OAuth error (empty error=terminal-denial is a local-DoS via known state) — fix or ledger.

MUTATION AUDIT GAP (orchestrator must close): the reviewer's sandbox denied socket bind (EPERM), so only 1/5 mutation reverts (constant-time state) was conclusively killed; the other 4 (verifier reuse, redirect-follow, single-flight bypass, token-before-vault) failed at bind, NOT confirmed. The orchestrator's socket-capable env must re-run these 4 and confirm they kill. GATE: the reviewer's env EPERM-blocked 124 socket tests; the orchestrator's full gate ran green (104 suites, 0 failed) with real sockets.

W5b.2 (metadata-fill, AFTER W5b.1): per the review's list — approved-registration immutable metadata (exact scopes/audience/resource/omission), real identity verification (JWKS/userinfo), refresh issuer/audience binding (P1-4), OpenAI codex-backend Responses Bearer origin, Anthropic api.anthropic.com Bearer + anthropic-beta:oauth (distinct from x-api-key), + the live secret sweep. Constants staged ~/haider-run/w5-oauth-provider-metadata.md.

# W5b release review

Scope verified at `96f0c00371d17561076b85e7b863da0543773c31`; final worktree is clean and byte-identical.

## Findings

### P0

None found.

### P1

1. **Detached refresh and callback tasks can outlive the daemon barrier.**  
   [oauth.rs:456](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:456), [oauth.rs:887](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:887), [oauth.rs:1630](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1630), [oauth.rs:1732](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1732), [runtime.rs:521](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/runtime.rs:521).  
   The coordinator start/callback and refresh-leader `tokio::spawn` handles are discarded; shutdown cancels coordinator flows but joins none of these tasks, and the broker has no shutdown/join mechanism. In a persistent runtime, a blocked old refresh can overlap a restarted daemon using the same rotating refresh token, triggering replay detection or token-family revocation. Callback tasks can also retain state/verifier/nonce/token buffers past `Stopped`.

2. **Late permanent refresh failures bypass the generation fence and can expire a replacement account.**  
   [oauth.rs:1824](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1824), [oauth.rs:1831](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1831), [oauth.rs:1846](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1846), [oauth.rs:1968](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1968), [accounts.rs:627](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:627).  
   `invalid_grant`, other permanent errors, and response-validation failures call alias-only `mark_expired` before comparing the captured fence. A delayed failure after remove/re-add or a newer same-alias generation can mark the replacement descriptor—even another provider/auth method—`Expired`. Existing coverage fences only late successful completion.

3. **A rotated-token vault failure does not guarantee durable expiration.**  
   [accounts.rs:692](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:692), [accounts.rs:699](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:699), [oauth.rs:1884](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1884).  
   After server-side rotation and failed vault persistence, `mark_refresh_expired` silently ignores failure to persist the descriptor status. The broker’s fence is process-local. On restart or the next resolve, the old, already-invalidated refresh token can be retried, contrary to §3.5’s no-retry/no-token-resurrection rule. The test substitutes an in-memory status actor and does not inject production descriptor persistence failure.

4. **Refreshed tokens are not issuer/audience/resource-bound.**  
   [oauth.rs:1995](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1995), [oauth.rs:2007](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:2007), [oauth.rs:2044](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:2044).  
   Refresh sends neither audience nor resource, ignores those fields if returned, and validates only bearer type, expiry, and scopes before copying the prior issuer/identity. A fake response containing an unexpected issuer/resource is accepted, violating the binding report’s terminal-mismatch rule. W5b.1 cannot safely add real subscription tokens until this is modeled and tested.

### P2

1. **Token-response bytes are not exclusively zeroizing.**  
   [oauth.rs:1259](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1259). Each ordinary `reqwest::Response::chunk()` allocation is copied into a `Zeroizing<Vec<u8>>`, but the original `Bytes` backing is released without scrubbing. The aggregate is protected; every process allocation containing token/error bytes is not.

2. **The required secret sweep is incomplete.**  
   [oauth_rpc_tests.rs:505](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/oauth_rpc_tests.rs:505), [oauth_rpc_tests.rs:578](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/oauth_rpc_tests.rs:578). It scans after orderly shutdown/WAL cleanup, captures no tracing or TUI output, never causes the fake endpoint to emit its `RAW_ERROR` sentinel, and does not check captured state/verifier against live success HTML. Transient WAL/temp/log leaks therefore remain invisible.

3. **Security coverage required by §7.2 is incomplete.**  
   [oauth_tests.rs:943](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth_tests.rs:943), [oauth_rpc_tests.rs:399](/Users/rizzist/haider-run/haider-agent/crates/haider-daemond/tests/oauth_rpc_tests.rs:399). Missing cases include state/nonce/callback-path independence, simultaneous callback replay, actual different-UID UDS access, initial-token-before-vault, late refresh failure after replacement, shutdown/restart refresh overlap, and production receipt/vault crash boundaries.

4. **Refresh-token omission can erase refreshability or erase a known expiry.**  
   [oauth.rs:2069](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:2069). With omission retention disabled, a missing refresh token is accepted and persisted as `None`. With retention enabled, the old token is retained but its finite absolute expiry becomes `None` when the response omits `refresh_expires_in`.

5. **The refresh CAS includes mutable status/selection fields.**  
   [accounts.rs:667](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/accounts.rs:667). A concurrent active/status change makes full-descriptor equality reject a successfully rotated response after the server invalidates the old token. Identity/generation changes should fence replacement; benign public-state changes should not silently strand the credential.

### P3

1. **A state-valid empty OAuth error consumes the flow.**  
   [oauth.rs:1170](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/oauth.rs:1170). `error=` is accepted as terminal `authorization_denied`, though §3.3 requires a standard nonempty OAuth error. A local process that knows state can cause denial of service; it cannot steal the code.

## §7.2 coverage

“Exercised” means a delivered test exists; socket-dependent execution was blocked in this review environment.

| Attack case | Delivered coverage |
|---|---|
| Success, S256, exact redirect | Exercised |
| Wrong/missing/duplicate state | Exercised in parser |
| Wrong path/Host/port | Exercised in parser |
| Non-GET | Exercised in parser |
| Oversized callback request | Exercised |
| Early malicious local connection | Exercised |
| State-valid user denial | Exercised |
| Code replay | Partial: sequential only |
| Verifier mismatch | Exercised |
| Callback timeout/cancel | Exercised |
| Token-endpoint redirect | Exercised; target-call count pinned at zero |
| Malformed/oversized initial token response | Exercised |
| Access expiry | Exercised |
| Rotating refresh token | Exercised |
| Concurrent refresh | Exercised |
| Caller cancellation during refresh | Exercised |
| `invalid_grant` | Exercised |
| Transient failure, still-valid access | Exercised |
| Transient failure, expired access | Exercised |
| Scope mismatch | Partial: initial exchange only |
| Issuer/audience/nonce mismatch | Initial exchange exercised |
| Refresh issuer/resource/audience mismatch | Missing and unsupported |
| Refresh malformed/oversized response | Missing |
| Refresh-token omission/expiry retention | Missing |
| Crash/failure before/after initial vault put | Partial unit reconstruction, no integrated crash |
| Refresh response before vault failure | Partial: fake status actor |
| Descriptor persistence failure after refresh vault failure | Missing |
| Late successful completion after remove | Exercised |
| Late failure after remove/re-add | Missing |
| Late completion versus newer vault generation | Exercised |
| Refresh during daemon shutdown/restart | Missing |
| Refresh during active/status change | Missing |
| Browser/card disconnect | Partial; waiting flow covered |
| Daemon restart requires fresh flow | Exercised |
| Remote start/status/cancel/add | Exercised synthetically |
| Actual non-same-UID UDS peer | Missing; production gate exists |
| Flow/ready-ref connection binding | Exercised |
| No public-client secret | Initial exchange exercised; refresh-specific assertion missing |
| No token usable before durable vault | Refresh exercised; initial account-add path missing |
| Independent state/nonce/path | Missing |
| Live WAL/temp/tracing/TUI sentinel sweep | Missing |

## Secret sweep and boundary audit

No formatted-error, receipt, descriptor, public status, or static browser-page leak was found by source review. The authorization URL is necessarily present only in the transient same-UID start response; its allocation and production codec buffers are zeroizing. Status responses cannot carry URL, code, state, verifier, nonce, tokens, or raw errors.

`OAuthAuthorizationWire`, `OAuthReadyRefWire`, `OAuthTokenBundleV1`, and `SecretHandle` have redacted formatting and zeroizing secret storage; the bundle has no derived clone/serde path and does not retain the ID token. The loopback listener is exact numeric `127.0.0.1:0`, with random path/state/verifier/nonce, exact GET/Host/path validation, 8 KiB callback and 256 KiB token bounds, deadlines, invalid-request caps, constant-time state comparison, PKCE S256, and no redirects.

However, because of P2-1/P2-2 and the sandbox bind denial, the headline “none anywhere the process touches” sweep is **not proven**.

The same-UID and capability gates are correctly layered: peer credentials are checked before framing at [connection.rs:875](/Users/rizzist/haider-run/haider-agent/crates/haider-daemon/src/connection.rs:875), negotiated UDS connections become `LocalSameUid` only afterward, and all four OAuth methods require Control plus the shared secret-surface gate. Ready claims are connection/instance/attempt-bound, claimed before actor `try_send`, restored on full mailbox, and zeroized on closed/drop paths. R7 no-await routing holds.

INV-1, INV-2, R9, R12, R13, and R14 show no source regression; durable account-add receipts are secret-free and reconcile before/after vault put. The task-ownership/barrier requirement does not hold because of P1-1.

## Mutation audit

All five claimed reverts were applied one at a time and restored:

| Mutation | Named test result |
|---|---|
| Constant-time state → `==` | **Killed** by `callback_state_comparison_is_constant_time_and_load_bearing` |
| Reuse verifier | Executed; test failed first at fake-server bind `EPERM` |
| Follow token redirect | Executed; test failed first at fake-server bind `EPERM` |
| Bypass single-flight | Executed; test failed first at fake-server bind `EPERM` |
| Return token before vault put | Executed; test failed first at fake-server bind `EPERM` |

Thus only 1/5 kills could be conclusively attributed to the mutation in this environment; the other four are not valid mutation confirmations. Final `git diff --exit-code` passed and HEAD remained exact.

## Gate

- Frozen commit: exact.
- Monotonic baseline: `927 → 965`; no deleted files/tests.
- `xtask test-count`: **965/965 pass**.
- `cargo fmt --all -- --check`: **pass**.
- `cargo clippy --workspace --all-targets -- -D warnings`: **pass**.
- Full workspace: all targets compiled; no `could-not-compile`. Ten test targets failed because TCP/UDS `bind(2)` returns `EPERM` in this sandbox—124 bind-dependent failures overall.
- Focused OAuth: **7 passed, 25 bind-blocked**.
- Daemond suites: **4 passed, 80 bind-blocked**.
- OAuth RPC: **0 passed, 2 bind-blocked**.
- `git diff --check`, final status, and byte-identity checks: **pass**.

The gate is therefore not green, although the execution failures are environmental rather than assertion failures.

## W5b.1 readiness

The sanctioned-provider table is correctly empty and unavailable providers return precise reasons without allocating flows. The provider-neutral bundle, coordinator, and credential-broker shape can support real variants after the P1 fixes, without replacing the architecture.

W5b.1 must add:

- Immutable approved registration metadata, exact scopes/audience/resource and refresh-omission semantics.
- Real signature/JWKS or authenticated-userinfo identity verification.
- Refresh issuer/audience/resource binding and corresponding fake-server negatives.
- OpenAI codex-backend Responses origin using the existing sensitive Bearer path.
- Anthropic `api.anthropic.com/v1/messages` Bearer mode plus `anthropic-beta: oauth`, kept distinct from the existing `x-api-key` mode.
- Shutdown-owned/joined refresh tasks, generation-fenced failure status updates, and durable rotation-failure tombstoning.
- Complete refresh, crash, different-UID, initial-persistence, mutation, and live secret-sweep tests.

Actual checked `Resolver` invocation remains explicitly assigned to W5c by §7.3; the additive `AuthExpired`/`RefreshFailed → RotationCause::Error` vocabulary is present without a fake rate-limit deadline.

VERDICT: NO_SHIP
