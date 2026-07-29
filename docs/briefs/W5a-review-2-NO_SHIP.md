# W5a.1 confirm — NO_SHIP (2 residual SSRF P1s; everything else CLOSED)

Reviewer: gpt-5.6 (codex), frozen dd535b1. The W5a.1 IP-literal origin fix is real but INCOMPLETE — two credential-exfil paths survive:

1. **P1 — literal-host-only origin check (resolve-time SSRF).** validate_compatible_origin only does host_str().parse::<IpAddr>(); it NEVER resolves hostnames. Any non-IP host is accepted and its RESOLVED address never validated — so an attacker HTTPS domain (valid cert) resolving to 169.254.169.254 / private / metadata / loopback is accepted and both probe + turn attach the bearer key. IP-literal encodings (octal/decimal/hex/IPv4-mapped-IPv6) ARE correctly canonicalized+blocked; the hole is hostnames.
2. **P1 — inherited HTTP proxy exfil.** The reqwest client omits .no_proxy(); with HTTP_PROXY set, the loopback-HTTP allowance (http://127.0.0.1:11434 Ollama) forwards the request + its Authorization header to the remote proxy — the key leaves loopback despite the URL check.

CONFIRMED CLOSED (do not touch): reasoning-continuation opaque round trip (faithful, session/branch/agent-scoped, durable across restart, foreign-provider fail-closed, UI-omitted, never text); incomplete-tool max-token → Finish(MaxTokens) no parse error (both Responses+Chat); bounded reads (/v1/models 1 MiB, errors 64 KiB, declared+chunked overflow rejected); overloaded_error → retryable Overloaded (mutation-verified); refusal → RefusalDelta+Finish(Refusal) never TextDelta; retry::never + redirect-refusal intact; report two-family amendment correct. 902→913, no deletions.

Required fix (W5a.2 — security only):
- **Resolve-validate-pin**: resolve the base-URL host's A/AAAA addresses; reject if ANY resolved address is forbidden (private 10/8·172.16/12·192.168/16, link-local 169.254/16·fe80::/10, metadata, ULA fc00::/7, and non-loopback for plain HTTP); PIN the validated address set through connection establishment so a rebind between validate-time and connect-time cannot redirect to a forbidden target (a reqwest custom dns_resolver that returns only validated addresses, or connect-to-pinned-IP with Host/SNI preserved). Loopback-HTTP stays allowed only when the resolved address IS loopback.
- **Disable inherited proxies** for credential-bearing transport (.no_proxy() on the compatible client) unless an explicit trusted-proxy policy exists; a plain-HTTP loopback request must not forward its Authorization header to an env proxy.
- Pins: an HTTPS host resolving to a private/metadata IP is rejected before any key-bearing request (inject a resolver stub — no real DNS needed); the HTTP_PROXY-set + loopback-base case does not send the Authorization header to the proxy. MUTATION CHECK on both.
- P3: the five new SSE fixtures have blank-line-at-EOF (git diff --check) — trim.

Note: the reviewer's sandbox denied UnixListener::bind (EPERM) so real-UDS + live-wire-capture couldn't run there; the orchestrator's env runs them. Tests must not depend on real DNS/proxy — inject the resolver + assert the header/target, don't hit the network.

## SSRF attack report — FAIL

The origin check is literal-host-only and does not satisfy the credential-confinement invariant.

| Vector | Result |
|---|---|
| `http(s)://169.254.169.254` | Rejected before transport/auth construction |
| `http(s)://10.0.0.1`, `192.168.1.1` | Rejected before transport/auth construction |
| Remote `http://203.0.113.7` | Rejected |
| `0x7f.0.0.1`, `0177.0.0.1`, `2130706433`, `127.1` | Accepted consistently as loopback |
| `[::1]`, `127.0.0.1` | Both accepted for HTTP |
| `::ffff:169.254.169.254`, `::ffff:10.0.0.1` | Rejected |
| `0.0.0.0` | Rejected |
| Encoded private forms `0x0a.0.0.1`, `012.0.0.1`, `167772161` | Canonicalized and rejected |
| Encoded metadata `0xa9fea9fe` | Canonicalized and rejected |
| `https://localhost:8443` | **Accepted; resolves to `::1` and `127.0.0.1`** |
| `https://localtest.me:8443` | **Accepted; DNS unavailable in this sandbox** |

No numeric non-loopback masquerade was found: validation and request construction use compatible URL parsing, and exotic loopback representations canonicalize correctly.

Literal-vs-resolved ruling: **literal-only, exploitable P1**. `validate_compatible_origin` merely attempts `host_str().parse::<IpAddr>()`; it performs no DNS resolution, resolved-address rejection, address pinning, or peer-address revalidation. An attacker-controlled HTTPS name with a valid certificate can resolve to a private, link-local, metadata, or loopback target, and both probe and turn paths then attach the bearer key.

A second P1 exists in the loopback-HTTP allowance: the reqwest client does not call `.no_proxy()`. Executing with:

```text
HTTP_PROXY=http://review-proxy.invalid:8080
NO_PROXY=
base_url=http://127.0.0.1:11434
```

showed the allowed loopback provider client inheriting the remote proxy matcher. Plain-HTTP proxying forwards the origin request and its `Authorization` header to that proxy, so the key can leave loopback despite the URL check.

Required fix: validate every resolved A/AAAA address, reject forbidden/special-use results, pin the validated address set through connection establishment to close rebinding, and disable inherited proxies for credential-bearing transport unless an explicit trusted-proxy policy exists.

## Reasoning-continuation round trip — confirmed

- The Responses fixture emits the complete reasoning item as `ProviderOpaque`.
- The reconstructed next request includes `reasoning.encrypted_content`, with the opaque item equal to the decoded provider item and not converted to text.
- Core does not interpret the encrypted content or unknown nested fields. It only wraps the JSON value with its provider key.
- Same-turn tool follow-up replay passed.
- An external-scratch test persisted a value containing an encrypted sentinel and unknown nested fields through the real SQLite store, closed/reopened it, and recovered an equal value.
- A different session did not receive the opaque block. Store reads are also branch/agent scoped, and foreign-provider blocks fail closed.
- It is value/field faithful, including the encrypted string bytes. Raw SSE whitespace and JSON key ordering are naturally not preserved because ingress parses into `serde_json::Value`.
- The durable extension is UI-omitted and never presented as ordinary assistant text.

## Incomplete-tool and P2 confirmation

- Responses max-token-mid-arguments: no `ToolCallStart`, args, or end crosses the adapter; terminal result is `Finish(MaxTokens)`.
- Chat max-token-mid-arguments: same result.
- `/v1/models` is capped at 1 MiB; error bodies at 64 KiB. Declared-length and chunked overflow attacks reject with typed errors.
- `overloaded_error` maps to retryable `Overloaded`.
- Responses and Chat refusals emit `RefusalDelta`, never `TextDelta`, followed by `Finish(Refusal)`.
- Concurrent-call, timeout, cancellation, refusal, reasoning, overload, and provenance fixtures passed.
- `reqwest::retry::never()` and redirect refusal remain intact.

## Mutation audit

- Origin-check removal: decimal-encoded `167772161` became canonical `10.0.0.1`, and the resulting request contained the sentinel bearer header. Restoring the check rejected it. The sandbox denied the mutation-only TCP proxy bind, so no false wire-capture claim is made.
- Mapping removal: deleting `overloaded_error → Overloaded` failed exactly with `InvalidRequest != Overloaded`; restoration passed.
- Both mutations were confined to external scratch. The frozen worktree was never modified.

## Gate and merge readiness

- Frozen `HEAD`: `dd535b189f39cc8dda371d46a069cf780271f214`.
- Final tracked worktree: clean, zero staged/unstaged modifications.
- Main `40d0a62` is an ancestor; branch is `0 behind / 3 ahead` and fast-forwardable.
- Focused delta: 11 test markers added, none removed; baseline `902→913`; `913/913` passes.
- Focused provider tests: `15/15`; provider unit tests: `5/5`; directed core opaque tests passed.
- Full workspace compile-only: passed; no `could not compile`.
- Clippy `-D warnings`: passed.
- Formatting: passed.
- Report amendment correctly records native Responses versus compatible Chat, with no fallback.
- Real-UDS execution was attempted: this sandbox rejects `UnixListener::bind` with `EPERM`. The daemond suite had 29 identical pre-Ready bind failures and one non-socket manifest pass; full workspace runtime similarly stopped at socket-dependent CLI tests. This is an environment block, not a code assertion failure.
- `git diff --check` additionally flags five new SSE fixtures for a blank line at EOF.

## New findings

- P1: HTTPS hostname-to-private/resolved-address bypass.
- P1: inherited HTTP proxy can exfiltrate credentials from the loopback-HTTP allowance.
- P2: none found.
- P3: five fixture blank-line-at-EOF warnings.

VERDICT: NO_SHIP
