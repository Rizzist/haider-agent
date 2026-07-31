# W5g-5 (a+b+c) — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w5-g5`, reviewed at e0c4820 (frozen ref).
Split: Fable took the card model field (5a) and the live-fix wave (5c);
the codex lane (gpt-5.6 xhigh) took provider/daemon discovery + routing
(5b, brief in docs/briefs/W5g-5b-custom-provider-brief.md).

## The wave's law: the LIVE probe is the review

A deterministic fake OpenAI-compatible server (loopback :18123, real
`/v1/models` + streaming `/v1/chat/completions`) and a PTY probe that
drives the WHOLE user journey: `+ Custom` card → name/origin/model →
`provider.configure` → chained key card → key validated LIVE → identity
pinned → **a real streamed turn**. 6/6.

Four blockers the unit suites could not see fell out of it, each now
pinned:

1. **One-shot create was impossible** — the live configure path
   validated the default against the bare discovery cache (empty for any
   NEW provider). The stated models are now the bootstrap inventory
   until discovery speaks; once it has run, discovery stays
   authoritative (the strict W5c.2b test is untouched and green).
2. **The chained key card was invisible** — the login card only rendered
   in the composer band, which `/accounts` never draws. The `+ … (API)`
   buttons had this latent trap since W5d: an open, total-modal,
   keystroke-eating card no frame drew. It now renders in the accounts
   footer.
3. **No validator for custom providers** — a custom chat-completions
   profile now validates with the SAME 1-token-turn law through
   `OpenAiCompatibleProvider` at its stored origin, under its declared
   default model.
4. **The session gate was a static builtin set** — an ENABLED custom
   chat-completions profile is creatable; it only exists because a
   durable, validated `provider.configure` committed it.

Plus 5b's core: `DiscoveredModel` from `{origin}/models` (openai-compat
`data[]`, windows honestly `None`), discovery-time origin backstop
(http = loopback only, https remote allowed, redirects off, bounded
body), family-routed turn adapters on the PROFILE origin (fixed-name
legacy path byte-identical), and the TUI's auto-discovery trigger now
serves ANY selected account (once per connection).

## Mutations (reviewer-chosen, EXECUTED post-commit)

| # | Mutation | Result |
|---|---|---|
| 5b-M1 | family-dispatch arm removed | KILLED (routing test) |
| 5b-M2 | loopback-http gate removed | KILLED (origin-policy test) |
| 5b-M3 | custom catalog source dropped | KILLED (refresh test) |
| 5b-M4 | credential base_url outranks profile | KILLED (routing test) |
| 5c-M1 | one-shot inventory fallback reverted | KILLED (registry test) |
| 5c-M2 | accounts login-card render dropped | KILLED (visibility test) |
| 5c-M3 | login-target family check dropped | KILLED (target test) |
| 5c-M4 | session-gate custom arm dead | KILLED by the LIVE probe (6/6 → 5/6, exactly the turn check) — no unit pin exists; the probe is the oracle of record |
| 5c-M5 | OAuth-only trigger restored | KILLED (trigger test) |

## Honest residuals (non-blocking)

- Custom discovery's client skips DNS pinning (the turn path's
  `CompatibleOriginGuard` is stronger) — hardening follow-up: reuse it.
- Custom-provider EDIT UI, HuggingFace card: still future patches.
- The session-gate custom arm has no in-repo unit pin (needs a hub
  harness with management wiring); the live probe covers it and is part
  of the release ritual for every custom-provider-touching patch.

## Gate

Workspace clippy `-D warnings` clean; host daemon suite green INCLUDING
the socket-bound tests codex's sandbox cannot run; full per-crate gate
`gate18.out`; ledger 1137 → 1146.

## Verdict

**SHIP** (merge to main, ships as v0.0.27 — custom providers WORK).
