# W-B — web search (provider-native per pair) + web fetch

Owner contract: "Web Search is core, implement as well (if possible via
provider, just use that per provider), Web Fetch also important.
implement as well." Authority: docs/research/wb-web-tools-research.md
(exact API shapes, verified 2026-08-06). Branch: `wb-web-tools`.
STATUS: STAGED — do not launch until the owner says continue.

## Capability matrix (LOCKED)

| Pair family | Search | Fetch |
|---|---|---|
| anthropic api-key + oauth | SERVER tool `web_search_20250305` | SERVER tool `web_fetch_20250910` + local fallback on refusal |
| openai api-key (Responses, non-lite) | HOSTED `{"type":"web_search"}` | LOCAL client tool |
| openai-oauth (responses-lite) | CLIENT function tool backed by POST {subscription_base}/alpha/search (codex-source-verified SearchRequest shape) — lite REJECTS hosted tools | LOCAL client tool |
| gemini 3.x models | `google_search` built-in (+ `url_context`) alongside function tools | url_context + LOCAL tool |
| gemini 2.5 / kimi / OSS / enterprise | none (tool not advertised — honest absence) | LOCAL client tool |

## Locked decisions

1. ANTHROPIC SERVER TOOLS: declare `web_search_20250305` (basic, all
   models, deliberately NOT the 2026 dynamic-filtering versions) with
   `max_uses: 8`, and `web_fetch_20250910` with
   `citations: {enabled: false}`, `max_content_tokens: 100000`,
   `max_uses: 10`. CRITICAL: `server_tool_use` /
   `web_search_tool_result` / `web_fetch_tool_result` blocks (incl.
   every `encrypted_content` and citation `encrypted_index`) must
   REPLAY VERBATIM within the turn — ride the G3 provider-opaque
   capture/replay machinery (same seam as thinking blocks; extend its
   laws). Error content is an OBJECT (web_search_tool_result_error with
   error_code), success is a LIST — decode both tolerantly. `pause_turn`
   stop_reason → resend the paused assistant message unchanged (law).
   Org-disabled tool → declared tool 400s: surface the provider error
   verbatim. OAuth third-party caveat goes in the notes verbatim from
   the research doc.
2. OPENAI HOSTED (api-key non-lite only): tool `{"type": "web_search",
   "search_context_size": "medium"}`; parse `web_search_call` output
   items + `url_citation` annotations. NEVER send hosted tools on lite
   (golden — the lite contract law set grows one clause).
3. OPENAI-OAUTH CLIENT SEARCH: a client function tool named
   `web_search` (schema {query: string}) advertised only on lite pairs;
   execution = daemon POST to `{subscription_base}/alpha/search` with
   the SearchRequest body from the research doc (settings:
   search_context_size medium, allowed_callers ["direct"],
   external_web_access true), same Bearer auth as turns; bounded result
   text into the tool_result. Unofficial endpoint: errors surface
   verbatim; a 404/410 marks the capability degraded for the session
   (no retry storm).
4. GEMINI: declare `google_search: {}` + `url_context: {}` ONLY on
   3.x-named models (the G3 name-gate pattern — 2.5 cannot combine
   built-ins with function declarations). Parse groundingMetadata
   (webSearchQueries, groundingChunks web.uri/title) and
   url_context_metadata into sources rendering; tolerant to absent
   fields.
5. LOCAL `web_fetch` CLIENT TOOL (all pairs; the universal capability):
   manifest {url, max_bytes?}; execution daemon-side through a NEW
   strict-public origin policy built on the G4a pinned-resolver guard:
   https to PUBLIC addresses only (http refused except loopback;
   RFC1918/link-local/metadata refused — model-supplied URLs are
   hostile input). Redirect cap 5, each hop re-validated through the
   guard. Content: text/* + application/json; text/html reduced to
   readable text (strip script/style/nav, keep headings/paragraphs/
   links/code; small in-crate reducer, no heavyweight dep), 96 KiB
   output cap with honest truncation marker; content-type refusal for
   everything else (PDF deferred to W-D). This IS an effect: broker
   class network-fetch, auto-allow under auto-mode, journaled
   intent/outcome with the URL.
6. RENDERING: server/hosted search calls surface as tool rows (query
   visible, results collapsed); a bounded "sources" line-list renders
   under the assistant message when citations/grounding exist (md +
   plain parity). No new full-screen UI this wave.
7. COST: search pricing NOT wired into /usage estimator this wave
   (documented); usage.server_tool_use counts pass through where
   reported.
8. Advertisement: per-turn tool list derives from the RESOLVED pair
   (R6) — switching pairs mid-session changes advertisement next turn
   (law). Subagents inherit the same derivation.

## Mandatory laws

- LW1 anthropic request golden: both server tools declared with exact
  shapes on anthropic pairs; absent on kimi/OSS.
- LW2 opaque replay: scripted stream with server_tool_use +
  encrypted_content result → follow-up request echoes verbatim (extend
  the G3 LT law family); pause_turn resend law.
- LW3 error-object vs success-list decode both paths (scripted SSE).
- LW4 lite NEVER carries hosted tools (golden) + client web_search
  advertised on lite only + alpha/search request-body golden.
- LW5 gemini: tools present on 3.x with function declarations, absent
  on 2.5 (both directions); groundingMetadata parsed into sources.
- LW6 local fetch origin matrix: public-https allowed; public-http,
  RFC1918, link-local, metadata refused; redirect hop re-validation
  (mutation anchor); size cap + truncation marker; html reduction
  drops script/style content (fixture).
- LW7 fetch journaled through the broker (intent/outcome with URL).
- LW8 per-turn advertisement follows the resolved pair after a switch.
- Regression: G3 thinking-replay laws untouched green; lite contract
  goldens extended not weakened.

## Discipline

Standard lane rules (CARGO_INCREMENTAL=0; per-crate tests; fmt each
commit; named paths; truthful ledger; notes + mutation-notes ≥6
executed kills covering: opaque echo, lite hosted-tool exclusion,
origin matrix both directions, redirect re-validation, html reduction,
pair-switch advertisement). NO real network calls in tests (scripted
SSE + loopback mock servers for local fetch). No version bumps/tags/
MCP/renames; never delete ~/.codex/sessions.
