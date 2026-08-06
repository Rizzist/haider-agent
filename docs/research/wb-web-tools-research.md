# W-B/W-D research — provider web tools + PDF (verified vs primary docs + codex source, 2026-08-06)

## Anthropic web_search (GA, no beta header)

- Versions: web_search_20250305 (basic, ALL models — W-B uses this);
  20260209/20260318 add code-execution dynamic filtering (4.6+ models,
  allowed_callers defaults to code_execution — avoid).
- Declaration: {"type":"web_search_20250305","name":"web_search",
  "max_uses":8, optional allowed_domains XOR blocked_domains (both=400),
  user_location{type:"approximate",city,region,country ISO-2,timezone}}.
- Response: {"type":"server_tool_use","id":"srvtoolu_*","name":
  "web_search","input":{"query":...}} then
  {"type":"web_search_tool_result","tool_use_id":...,"content":[
  {"type":"web_search_result","url","title","encrypted_content",
  "page_age"}]}. ERROR: content is an OBJECT
  {"type":"web_search_tool_result_error","error_code": too_many_requests
  |invalid_tool_input|max_uses_exceeded|query_too_long|
  request_too_large|unavailable} (HTTP still 200). Empty list = zero
  results, not error.
- MULTI-TURN: echo assistant content back EXACTLY incl. every
  encrypted_content and citation encrypted_index — missing/modified →
  400. Citations on text blocks: {"type":"web_search_result_location",
  url,title,encrypted_index,cited_text≤150}.
- Streaming: server_tool_use block start → input_json_delta (query) →
  pause → web_search_tool_result as content_block_start. Long turns may
  end stop_reason "pause_turn" → resend paused assistant msg unchanged.
- Pricing $10/1k searches; usage.server_tool_use.web_search_requests.
  Org admins can disable (declared tool then 400s).
- OAuth: Claude Code itself sends web_search over subscription OAuth
  (works de facto); Feb-2026 policy bans third-party subscription auth
  — documented caveat, owner-accepted risk posture same as turns.

## Anthropic web_fetch (GA, no beta header)

- Versions: web_fetch_20250910 basic (W-B uses); 20260309 +use_cache;
  20260318 +response_inclusion.
- Declaration: {"type":"web_fetch_20250910","name":"web_fetch",
  "max_uses":10, allowed/blocked_domains, "citations":{"enabled":bool},
  "max_content_tokens":100000}.
- Result: {"type":"web_fetch_tool_result","tool_use_id",
  "content":{"type":"web_fetch_result","url","content":{"type":
  "document","source":{"type":"text","media_type":"text/plain",
  "data":...},"title"},"retrieved_at"}}. PDFs come back as source
  base64 application/pdf. Errors object form, codes incl.
  url_too_long(250), url_not_allowed, url_not_in_prior_context,
  url_not_accessible, unsupported_content_type, max_uses_exceeded.
- HARD RULE: only fetches URLs already present in conversation context
  (never model-constructed). No JS rendering. Free (token costs only).
- OAuth availability UNVERIFIED (CC uses local fetch, not server) —
  degrade honestly on 400 → local tool fallback.

## OpenAI Responses web_search

- Hosted tool type "web_search" (legacy "web_search_preview"):
  {"type":"web_search","search_context_size":"low|medium|high",
  user_location{type approximate,...}, filters{allowed_domains,
  blocked_domains ≤100}, external_web_access bool,
  search_content_types, image_settings, return_token_budget}.
- Output: {"type":"web_search_call","id":"ws_*","status","action":
  {"type":"search|open_page|find_in_page", query|url|pattern}}; message
  annotations [{"type":"url_citation",start_index,end_index,url,title}].
  include options: "web_search_call.action.sources",
  "web_search_call.results".
- Pricing: reasoning models $10/1k + content tokens; non-reasoning
  $25/1k.
- RESPONSES-LITE (codex subscription) REJECTS ALL HOSTED TOOLS (codex
  source spec_plan.rs returns empty hosted specs on lite). Codex
  instead ships client namespaced tool web.run and executes search
  itself via POST {provider_base}/alpha/search (chatgpt.com/backend-api
  /codex/alpha/search), same OAuth Bearer, body SearchRequest {id
  (session), model, reasoning, input (recent history items), commands,
  settings{user_location, search_context_size, filters{allowed_domains},
  allowed_callers:["direct"], external_web_access}, max_output_tokens}.
  --search == config web_search="live"; modes disabled|cached|live|
  indexed (cached → external_web_access:false; indexed adds
  indexed_web_access:true). UNOFFICIAL endpoint — verified from codex
  main 2026-08; surface errors verbatim, degrade on 404/410.

## Gemini (v1beta generateContent)

- tools: [{"google_search": {}}, {"url_context": {}}] — current models
  (2.0+, all 3.x). Gemini 3 models COMBINE built-ins with
  function_declarations; 2.5 does NOT (mixing unsupported — omission-
  inferred). W-B: declare on 3.x-named models only.
- groundingMetadata: {webSearchQueries[], searchEntryPoint
  {renderedContent}, groundingChunks[{web:{uri,title,domain}}],
  groundingSupports[{segment{startIndex,endIndex,text},
  groundingChunkIndices[],confidenceScores[]}]}. url_context:
  candidates[].url_context_metadata.url_metadata[] {retrieved_url,
  url_retrieval_status}. Limits: 20 URLs/req, 34MB/URL; retrieved
  content billed as input tokens. ToS: display searchEntryPoint
  renderedContent when showing grounded answers.
- Quotas: 3.x — 5k free searches/mo then $14/1k per executed query;
  2.5 — 1500/day free then $35/1k grounded prompts.

## PDF input (W-D)

- ANTHROPIC document block (no beta for base64/url):
  {"type":"document","source":{"type":"base64","media_type":
  "application/pdf","data":...}} | {"type":"url","url"} | file_id via
  Files API (beta header files-api-2025-04-14). Optional title,
  context, citations{enabled} (page_location cites), cache_control.
  Limits 32MB/request, 600 pages (100 under-1M-context). All active
  models; PDFs before text. Tokens: text (~1500-3000/page) + page
  images at image rates.
- OPENAI Responses input_file: {"type":"input_file","file_id"} |
  {"filename","file_data":"data:application/pdf;base64,..."} |
  {"file_url"}. purpose "user_data"; 50MB/file and /request; vision
  models (all gpt-5.x). Lite: UNCONFIRMED — assume unsupported, use
  extraction fallback.
- GEMINI: parts inline_data {mime_type application/pdf, data b64}
  (≤20MB total payload) or file_data via File API (48h, up to 50MB/
  1000 pages). 258 tokens/page default.
- Fallback (kimi/OSS/lite): local pure-Rust text extraction into the
  G2 File inlining lane, capability-gated per pair like vision.

## Flagged/unconfirmed

(a) Anthropic server tools over subscription OAuth: de-facto works for
CC traffic, ToS-banned for third parties since Feb 2026. (b) codex
alpha/search endpoint unofficial (source-verified only). (c) gemini
url_context field casing + 2.5 mixing restriction omission-inferred.
(d) OpenAI PDF page-limit removal is a docs change only.
