# Lane providerrebind — per-session provider rebind lever + declared caching/reuse behaviour (v0.0.970, M)  Branch lane-970-providerrebind.
Read docs/testing/v0.0.970/LANE-COMMON.md FIRST. AHRB item 6 (owner-approved 2026-09-03): benchmarks need deterministic per-row routing of a session's
provider traffic to a specific base_url without restarting the daemon, and a declaration of caching/reuse so economy numbers compare daemon vs stateless fairly.
DELIVER: (1) an RPC + CLI verb that rebinds ONE session's provider endpoint: `haider session provider rebind --session <id> --provider <id> [--base-url
<url>] [--account <name>]` (RPC `session.provider.rebind`), taking effect on the next request of that session, journaled as a durable, replayable session
event (additive kind, announced in the changelog), validated against the registry (unknown provider/account → typed error; base_url override allowed only for
providers that permit it or for test/proxy providers — say which); (2) `status --json` gains `daemon.caching` = {prompt_cache: bool, provider_view_cas:
bool, session_reuse: "resident"|"one_shot", idle_ttl_ms} so a harness can annotate economy rows; document the semantics in automation-contract-v1.md.
Tests: rebind changes the next request's target (fake proxy ledger), earlier in-flight requests unaffected, replay parity (the event replays), typed
errors, status field golden. No wall regression (warm ABBA within MAD). `bash run.sh test` green; docs/testing/v0.0.970/providerrebind.md. LAST line: SHIP or NO_SHIP.

NOTE (AHRB 2026-09-03): declare `daemon.caching.cache_regime` = "automatic-prefix" for openai-family adapters and "explicit-breakpoints" for Anthropic (cache_control ephemeral), so a harness reads 0 breakpoints correctly.
