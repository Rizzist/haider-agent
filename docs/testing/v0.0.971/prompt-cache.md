Prompt-cache reuse contract (lane 971-prompt-cache)

Fable/Mythos 5 numeric minor releases use the same cache placement family;
strict dated/Vertex suffix parsing is retained, while arbitrary labels and
other major releases remain unsupported. This normalization is cache-specific:
it does not guess effort, computer-use, or model catalog capabilities. The
provider catalog does not expose the cache threshold/TTL contract, so the
source-backed cache policy is the fallback. The first-party OAuth route now
uses that policy too. Custom compatible routes still need their explicit
capability override and matching account/provider metadata.

Source checks on 2026-09-08:

- https://platform.claude.com/docs/en/build-with-claude/prompt-caching documents
  Fable/Mythos 5.1 support, a 512-token minimum, and 0.025x reads. The existing
  OAuth-only beta header composition remains scoped to actual 1h markers.
- https://developers.openai.com/api/docs/models/gpt-6-astra and
  https://developers.openai.com/api/docs/guides/prompt-caching establish Astra
  caching. Exact Astra IDs and strict snapshots receive a routing key; this
  does not enable the public explicit-cache dialect on subscription lite.
- https://ai.google.dev/gemini-api/docs/caching includes Gemini 3.8 Flash's
  4096-token threshold. GenerateContent retains the explicit resource path;
  the Interactions API is not used here.

Canonical tool schema ordering and the existing four-marker planner remain.
The Anthropic provider prefix order is tools, system, then history. Dynamic
accepted-turn tails can change without invalidating earlier system/tool bytes.
Per-request run/turn/attempt correlation remains independent of reusable cache
identity. The optional `request_view_epoch` retains run/snapshot changes for
the exact provider-view ledger and its strict middle-mutation check. It never
enters OpenAI routing keys or Gemini resource identity. Older callers omit it
and retain the cache epoch as their view epoch. Account, provider, auth, model, output budget, system/tool/effort,
route and compaction identity remain hard boundaries. OpenAI's existing fork
cohort validation remains authoritative. Gemini additionally compares rendered
headers and exact inherited history before reusing a session-owned resource;
web-tool requests cannot reuse a previously created explicit resource. Prepared
reply-arena bindings are resolved when hashing the resource's history and when
creating its cold prefix; internal markers never become cached model text.

Usage reports retain the existing per-request normalized counters and their
availability fields. Cache-control observations now use actual emission,
including the API-key string-to-block system conversion. New omission enum
values decode as Unknown on older readers; no RPC methods are added.

Reproducible synthetic evidence

The provider's opt-in `test-support` feature exposes the recording HTTP fake
for daemon tests without cross-crate source inclusion. It only uses supplied
synthetic credentials and loopback sockets. Its cache keys derive from exact
received prefix blocks, model and account. OAuth beta/version headers, legal
markers and a minimum prefix are checked before any write/read. Changing tool
arguments named `cache_control` must still change the logical prefix.

Set HAIDER_CACHE_EVIDENCE_DIR to a private directory and run the provider's
`oauth_cache_fake_checks_wire_headers_prefixes_and_invalidation` test and the
daemon's `prompt_cache_http_actor_journal_usage_report_conserves_two_turn_and_tool_reads`
test. The latter runs the same eight-turn/tool-loop script with controls off
and on, exercises the real adapter, actor, SQLite journal and usage.report
fold, and records sanitized headers, synthetic bodies, prefix hashes and
per-request normalized usage. A controls-off run is a synthetic counterfactual,
not a measurement of the historical installed candidate.

Use scripts/qa-gate/economydiet_measure.py --cache-capture <capture.json>
--dialect anthropic-messages --bench-root <AHRB-root> --output <measurement.json>
for independent reference-token, schema, fixed-prefix and cache-counter splits.
Gemini captures use --dialect gemini-generate-content and must include each
cached resource's exact `cached_prefix`; measuring only the suffix is refused.
The fake uses byte/4 units. Cost arithmetic over those units is illustrative;
it is not provider billing or measured live savings. The historical 8-turn,
17-tool-call economy fixture remains separate from this new script.

All Cargo/Gradle/xtask commands must use the harness build-slot wrapper with a
worktree-local CARGO_TARGET_DIR. Run fmt, workspace Clippy with warnings denied,
provider/core/daemon/protocol suites, wire goldens, test-count and unsafe gates.
Do not change the RPC method pin or Android unsafe ceiling. No platform-specific
or durability policy changes are part of this lane.

Separate verification must bind the candidate/tree/build hashes and rerun
cross-turn interaction tests on the integrated grants/redaction/monitor-wake
candidate. Exercise the real CLI and usage.report, then perform the
owner-authorized repeated live provider request without copying credentials
or authorization headers. Synthetic reads cannot substitute for that live
provider read counter or the historical fixture rerun. Only the separate
Astra verifier can issue SHIP.
