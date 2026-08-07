# W-B web tools — mutation notes

Every mutation below was **EXECUTED**, not reasoned about. The protocol for
each row was:

1. commit first (clean tree — a mutation never runs beside uncommitted work);
2. apply ONE single-anchor edit with `python3` asserting `src.count(old) == 1`;
3. run the ONE named test and require `running 1 test` in the output (vacuity
   check — a filtered-out test proves nothing);
4. record the OBSERVED runtime failure verbatim;
5. `git checkout --` the file and re-run the same named test green.

“Expected RUNTIME failure” below is the failure that was actually observed;
compile-only breakage is never the claimed evidence.

| # | Area | Production mutation (single anchor) | Named test (`running 1 test` verified) | OBSERVED runtime failure |
|---|---|---|---|---|
| 1 | Opaque echo (LW2) | `haider-provider/src/wire/mod.rs` — in `content_block`, the Anthropic provider-opaque arm `Ok(data.clone())` re-serializes a filtered copy that removes `encrypted_content` and unknown fields. | `haider-provider --test anthropic_provider_tests server_tool_facts_replay_verbatim_and_cited_text_dedups_normalized_history` | `assertion left == right failed: the result block — encrypted_content and unknown fields included — replays VERBATIM` — the replayed `web_search_tool_result` lost `"future_field": "kept"`. |
| 2 | Lite hosted-tool exclusion (LW4) | `haider-provider/src/openai.rs` — `if hosted_web_search && !codex_responses_lite {` → `if hosted_web_search {`. | `haider-provider --lib hosted_web_search_declares_on_api_key_and_never_on_lite` | `lite never carries hosted tools regardless of the flag: {...,"tools":[{"search_context_size":"medium","type":"web_search"}]}` — the hosted tool reached a responses-lite body. |
| 3 | Origin matrix, both directions (LW6) | `haider-provider/src/webfetch.rs` — in `validate_fetch_target`, the https branch's `blocked_public_web_target(ip)` refusal block deleted (the loopback/plain-HTTP rule left intact). | `haider-provider --lib origin_matrix_allows_public_and_refuses_hostile_targets_both_directions` | `https://10.23.45.67/ must be refused: ()` — the RFC1918 target was admitted. |
| 4 | Redirect re-validation (LW6) | `haider-provider/src/webfetch.rs` — `fetch_public_url_with_resolver`'s per-hop `validate_fetch_target(...).await?` becomes first-hop-only (later hops fall back to an unvalidated `ValidatedFetchTarget`). | `haider-provider --test webfetch_tests redirect_hops_are_revalidated_through_the_origin_fence` | `assertion left == right failed: the hop is refused by the fence, not by the network: Transport: web_fetch transport failed: error sending request for url (http://169.254.169.254/latest/meta-data/)` — `left: Transport`, `right: InvalidRequest`. |
| 5 | HTML reduction (LW6) | `haider-provider/src/webfetch.rs` — `reduce_html_to_text`'s `DROP_CONTENT` list loses `"script"` and `"style"`. | `haider-provider --lib html_reduction_drops_script_style_nav_and_keeps_readable_structure` | `script content dropped` — inline script text survived into the reduced output. |
| 6 | Pair-switch advertisement (LW8) | `haider-daemon/src/worker.rs` — `start_turn`'s `config.tools = advertised_tool_definitions(..., &resolved.provider_name, ...)` pinned to the constant `OPENAI_OAUTH_PROVIDER_NAME` (the session's ORIGINAL pair) instead of the pair resolved for THIS turn. | `haider-daemon --lib pair_switch_reshapes_the_web_tool_advertisement_on_the_next_turn` | `the client search does not follow the session off responses-lite` — after committing a switch to `anthropic-oauth`, turn 2 still advertised `web_search` (and `web_fetch`). |
| 7 | alpha/search request body (LW4, M4) | `haider-provider/src/openai.rs` — `codex_alpha_search_request_body`'s `"external_web_access": true` → `false` (the codex `cached` mode). | `haider-provider --lib alpha_search_request_body_is_golden` | `assertion left == right failed` — `settings.external_web_access: Bool(false)` vs the golden `Bool(true)`. |
| 8 | Gone-endpoint session degrade (LW4, M4) | `haider-daemon/src/worker.rs` — the `WebSearch` dispatch arm's `if failure.degraded { ...degrade_openai_alpha_search(...) }` latch deleted (the typed failed result kept). | `haider-daemon --lib a_gone_alpha_search_endpoint_degrades_the_session_for_the_next_turn` | `turn 2 must not re-offer the gone capability` — after a 404 the next turn still advertised `web_search`, i.e. the retry storm the decision forbids. |

## Notes on kill 4 (why the law had to be sharpened first)

Kill 4 was run against a **strengthened** version of the law. As originally
committed in M3a the redirect law only asserted
`error.message.contains("web_fetch")`, and the first-hop-only mutation
SURVIVES that: with the fence bypassed the fetch still fails — as a
`Transport` error from *dialing* the hostile target — and a transport error's
message also contains `web_fetch`. That is the degenerate-observer class.

Two changes made the observer discriminating, committed as `141d0e5` BEFORE
the mutation ran:

- assert `error.kind == ProviderErrorKind::InvalidRequest`, so “refused by the
  fence” and “failed at the socket” are distinguishable;
- swap the public plain-HTTP redirect fixture from `93.184.216.34` (a real
  routable address) to `198.51.100.7` (TEST-NET-2, RFC 5737), so that even a
  DELIBERATELY BROKEN fence cannot turn a fixture into a real network call.
  The remaining hostile fixtures (`169.254.169.254`, `10.0.0.8`) are likewise
  unroutable.

## Not claimed

- No mutation was applied to the pre-existing G3 thinking-replay machinery,
  the responses-lite contract goldens, or the W-A background-task laws. Those
  are regression surfaces for this wave, verified green, not mutation targets.
- LW7 (broker journal for `web_fetch`) has runtime observers
  (`web_fetch_journals_intent_and_outcome_with_the_url`,
  `live_web_fetch_is_brokered_journaled_and_refusals_stay_typed_results`) but
  no executed kill this wave; the campaign's six mandated areas plus the two
  M4 areas were prioritized.

## Review of record (coordinator, executed post-lane, Opus 4.8)

Read the full branch diff (provider web-tool declarations + opaque
capture, fetch engine, daemon advertisement/broker, 30+ laws). Two
structural seams probed:

1. **Cross-provider strip of the NEW web-search opaque facts** — SAFE BY
   CONSTRUCTION and already observed: the anthropic web facts are
   captured as `StreamEvent::ProviderOpaque` tagged `anthropic`, and
   `strip_foreign_provider_opaque` (worker.rs) matches on
   `Block::ProviderOpaque { provider }` with NO kind discrimination, so
   the G3 `cross_provider_switch_strips_foreign_opaque_facts` law already
   pins the exact code path. A web-search-specific strip law would kill
   no mutation the G3 law doesn't — not written (no theater law; the P1
   render-walk lesson).

2. **The 4 MiB SOURCE cap** (`read_body_bounded`, distinct from the
   96 KiB OUTPUT cap) — GENUINE UNOBSERVED GATE. It bounds the raw body
   BEFORE html reduction (memory safety against an unbounded body), and
   NO law observed it. The trap: `truncated` alone is degenerate (the
   output cap sets it too — the same class as the lane's own kill-4
   catch). Closed with a NON-degenerate pin
   `oversized_source_is_capped_before_reduction_even_when_it_reduces_small`:
   5 MiB of dropped `<script>` content around a tiny marker hits the
   source cap but reduces far under the output cap, so `truncated`
   reflects the SOURCE cap alone. Kill-executed: neutering the clamp
   flips `truncated` to false ("running 1 test" observed), reverted,
   6/6 green.

Lane's 8 kills spot-checked against the notes; the kill-4
degenerate-observer sharpening is exemplary. Ledger 2077 -> 2078 with
the review pin. Campaign ACCEPTED.
