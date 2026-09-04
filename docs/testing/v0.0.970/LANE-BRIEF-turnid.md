# Lane turnid — stable per-turn correlation id on every model request (v0.0.970, M)  Branch lane-970-turnid.
Read docs/testing/v0.0.970/LANE-COMMON.md FIRST. AHRB finding (2026-09-03): benchmarks classify primary vs side-channel model requests by sniffing a marker
out of prompt text; it does not survive prompt handling (100/100 row-43 requests misclassified). DELIVER: every outbound provider request carries a declared
correlation id: HTTP header `X-Haider-Turn: <session_id>/<run_id>/<turn_ordinal>/<request_ordinal>` plus `X-Haider-Request-Kind: primary|side` (side =
delegation/summarization/estimation/tool-support requests), on all provider adapters (openai-compatible, anthropic, gemini, …) — headers only by default
(never inside the JSON body of strict provider schemas); an opt-in body `metadata` mirror for adapters that declare support. The same ids must appear in
the journal's request-attempt marker and the HAIDER_DAEMON_TRACE records so attribution joins exactly. Contract doc: docs/automation-contract-v1.md
(new section) + event-schema-changelog. Tests: header presence/shape on each adapter with the fake proxy ledger; kind classification pinned for a
delegation turn and a tool turn; no change to request bodies (golden). Zero measurable wall cost (warm ABBA within MAD). `bash run.sh test` green;
docs/testing/v0.0.970/turnid.md. LAST line: SHIP or NO_SHIP.

LOCKED HEADER NAMES/VALUES (pinned by the AHRB adapter): `X-Haider-Turn: <session_id>/<run_id>/<turn_ordinal>/<request_ordinal>` (turn_ordinal 1-based
monotonic per session; request_ordinal 1-based within the turn so retries are N/2, N/3…; unsigned ints, no padding) and `X-Haider-Request-Kind:
primary | side | warmup` (lowercase; `warmup` only for explicitly unmeasured warm-up requests, otherwise omit that value).
