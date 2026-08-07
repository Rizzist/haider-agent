# W-B — web search + web fetch — implementation notes

Owner ask: "Web Search is core, implement as well (if possible via
provider, just use that per provider), Web Fetch also important."
Delivered per `WB-web-brief.md`; API shapes from
`docs/research/wb-web-tools-research.md`. Branch `wb-web-tools` off
v0.0.78. Lane completed across three interruption/handoff boundaries
(two Fable session/quota deaths, one stream stall); coordinator finished
the notes + ledger on Opus 4.8.

## What shipped

### Search — provider-native per pair (the brief's capability matrix)
- **anthropic (api-key + oauth)**: server tool `web_search_20250305`
  declared (max_uses 8), basic version deliberately — NOT the 2026
  code-execution dynamic-filtering versions. `server_tool_use` /
  `web_search_tool_result` blocks captured and REPLAYED VERBATIM within
  the turn (every `encrypted_content` + unknown field preserved) through
  the G3 provider-opaque seam; `pause_turn` resends the paused assistant
  message unchanged; error content (object) vs success (list) decoded on
  both paths. Cited text surfaces as bounded sources rows.
- **openai api-key (Responses, non-lite)**: hosted `{"type":
  "web_search","search_context_size":"medium"}`; `web_search_call`
  captured verbatim, `url_citation` annotations become sources. NEVER
  emitted on responses-lite (the lite contract golden grew one clause).
- **openai-oauth (responses-lite)**: since lite rejects ALL hosted
  tools, a CLIENT function tool `web_search` is advertised on lite pairs
  only; the daemon executes `POST {subscription_base}/alpha/search` with
  the codex-verified SearchRequest body (external_web_access true,
  allowed_callers ["direct"], search_context_size medium) using the same
  Bearer credential. A 404/410 latches the capability DEGRADED for the
  session (no retry storm). Executor sits behind injected credential +
  HTTP seams so it is law-tested without a network.
- **gemini**: `google_search` + `url_context` built-ins declared ONLY on
  3.x-named models (2.5 cannot combine built-ins with function
  declarations — the G3 name-gate pattern); groundingMetadata +
  url_context_metadata parsed tolerantly into rows/sources.
- **kimi / OSS / enterprise / gemini-2.5**: no search tool advertised —
  honest absence, not a stub.

### Fetch — universal local tool on every pair
- `web_fetch` client tool on every pair EXCEPT first-party anthropic
  (which keeps the server `web_fetch_20250910` name, falling back to the
  local tool when the server tool refuses — decision 1 latch).
- Engine: daemon-side GET behind a NEW strict-public origin fence built
  on the G4a pinned-resolver guard — public HTTPS only, loopback HTTP
  allowed, RFC1918 / link-local / metadata REFUSED as `InvalidRequest`.
  5-hop redirect cap, EACH hop re-validated through the fence. text/* +
  application/json admitted; HTML reduced to readable text by an
  in-crate reducer (drops script/style/nav, keeps headings/paragraphs/
  links/code); 96 KiB output cap with an honest truncation marker.
- Brokered as a `Network{host}` effect (Ask default; the empty-host rule
  is the family wildcard; exec-override auto-allows under auto-mode);
  intent + outcome journaled with the URL.

### Advertisement
- The per-turn tool list derives from the RESOLVED pair (R6). Switching
  pairs mid-session reshapes the web-tool advertisement on the NEXT turn
  (pinned live). Subagents inherit the derivation via the delegation
  grant.

## Law inventory (all green by name)
Provider: `web_tools_declaration_is_exact_and_absent_without_the_flag`,
`server_tool_facts_replay_verbatim_and_cited_text_dedups_normalized_history`,
`hosted_web_search_declares_on_api_key_and_never_on_lite`,
`hosted_web_search_call_captures_verbatim_and_citations_surface_as_sources`,
`web_builtins_declare_on_3x_beside_function_declarations_and_never_on_25`,
`grounding_metadata_decodes_into_rows_and_sources_tolerantly`,
`gemini_web_builtins_supported`,
`alpha_search_request_body_is_golden`,
`origin_matrix_allows_public_and_refuses_hostile_targets_both_directions`,
`redirect_hops_are_revalidated_through_the_origin_fence`,
`html_reduction_drops_script_style_nav_and_keeps_readable_structure`,
`web_fetch_manifest_declares_the_network_family_and_url_schema`.
Daemon:
`web_fetch_advertises_on_every_pair_except_first_party_anthropic`,
`web_fetch_asks_by_default_and_the_empty_host_rule_is_the_family_wildcard`,
`web_fetch_journals_intent_and_outcome_with_the_url`,
`live_web_fetch_is_brokered_journaled_and_refusals_stay_typed_results`,
`pair_switch_reshapes_the_web_tool_advertisement_on_the_next_turn`,
`a_gone_alpha_search_endpoint_degrades_the_session_for_the_next_turn`.
Mutation campaign: 8 executed kills — see `WB-web-mutation-notes.md`
(incl. the kill-4 degenerate-observer catch: the redirect law was
sharpened to assert the `InvalidRequest` KIND before the mutation, since
a first-hop-only fence otherwise "passes" by failing as a transport
error).

## Deviations / gaps (disclosed)
1. Search cost is NOT wired into the /usage estimator this wave (brief
   decision 7); `usage.server_tool_use` counts pass through where the
   API reports them. Follow-up if the owner wants search billing.
2. Anthropic `web_fetch` server tool is declared but its OAuth
   availability is UNVERIFIED (Claude Code uses a local fetch, not the
   server tool) — the local-fetch fallback latch (decision 1) covers a
   refusal honestly.
3. Subscription-OAuth server-tool traffic carries the same Feb-2026
   third-party-auth ToS caveat as ordinary turns (research doc §flagged)
   — owner-accepted posture, unchanged from the turn path.
4. The `alpha/search` endpoint is unofficial (codex-source-verified,
   not documented); errors surface verbatim and 404/410 degrades — no
   fabricated resilience.
5. LW7 has runtime observers but no executed kill this wave (the six
   mandated areas + two M4 areas were prioritized) — disclosed in the
   mutation notes' "Not claimed" section.

## Regression surfaces verified green (not mutation targets)
G3 thinking-replay laws, responses-lite contract goldens, W-A
background-task laws — all green post-wave.
