# Patch brief W2b/C1.1 — review round-1 fixes (Anthropic adapter)

Worktree /Users/rizzist/haider-run/haider-c1, branch w2b-c1. Findings in
docs/briefs/C1-review-1-NO_SHIP.md (2 High, 2 Medium, 2 Low). Fix ALL:

1. HIGH no hidden retries: construct the reqwest client with retries disabled
   (`.retry(reqwest::retry::never())` or the 0.13 equivalent) — the ACTOR owns backoff;
   the adapter must surface every failure exactly once. Add a test asserting the client
   config (or a doc-tested constructor invariant if config is not inspectable).
2. HIGH bounded stall behavior: connect timeout (~10s) AND a per-chunk read deadline on
   the SSE stream (~90s idle → typed retryable Transport error). The idle deadline wraps
   the chunk await (tokio timeout), not the whole turn (long generations are legal).
   Test with a fixture stream that hangs mid-turn under a paused clock.
3. MEDIUM promotion/manifest coherence: the offline replay test must accept BOTH
   provisional and promoted manifests (assert the flag exists and fixtures parse; never
   hardcode provisional==true or a fixture COUNT that promotion changes). Promotion must
   preserve every shape in the manifest (seven incl. mid-stream overload) or fail loudly.
4. MEDIUM semantic promotion gates: each captured shape must pass a shape-specific
   assertion before replacing a fixture (text→AgentText deltas end_turn; tool_call→
   ToolCall args json; usage_heavy→usage frame fields; 429→RateLimit typed error w/
   retry-after; overload→mid-stream error; malformed→MalformedFrame). No capture, no swap.
5. LOW oracle fidelity: make implementation match the oracle — content_block_start for
   thinking emits NOTHING (buffer; deltas carry content). If you believe the start can
   carry text, change the ORACLE with a documented citation instead, and test both ways.
6. LOW strip trailing blank lines at EOF in all .sse fixtures; git diff --check clean.

Gate: cargo test --workspace, clippy -D warnings (all targets), fmt --all --check,
xtask test-count --update, git diff --check. Leave changes uncommitted.
